use crate::protocol::v2::CommandAction;
use crate::protocol::v2::CommandExecutionSource;
use crate::protocol::v2::CommandExecutionStatus;
use crate::protocol::v2::ThreadItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_path_uri::LegacyAppPathString;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

/// Reconstructs command execution items from raw tool calls in non-paginated rollouts.
///
/// Paginated rollouts persist canonical command items. Non-paginated rollouts instead persist the
/// raw `exec_command` and `write_stdin` call/output pairs, while their richer lifecycle events
/// remain transient. This projector keeps those raw pairs visible when thread history is rebuilt.
#[derive(Default)]
pub(super) struct NonPaginatedExecHistory {
    default_cwd: Option<LegacyAppPathString>,
    turn_cwds: HashMap<String, LegacyAppPathString>,
    commands: HashMap<String, NonPaginatedExecCommand>,
    process_commands: HashMap<String, String>,
    write_stdin_processes: HashMap<String, String>,
}

pub(super) struct NonPaginatedExecItemUpdate {
    pub(super) turn_id: Option<String>,
    pub(super) item: ThreadItem,
}

#[derive(Clone)]
struct NonPaginatedExecCommand {
    call_id: String,
    turn_id: Option<String>,
    command: String,
    cwd: LegacyAppPathString,
    process_id: Option<String>,
    status: CommandExecutionStatus,
    aggregated_output: String,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
}

#[derive(Deserialize)]
struct ExecCommandArgs {
    cmd: String,
    workdir: Option<String>,
}

#[derive(Deserialize)]
struct WriteStdinArgs {
    session_id: i32,
}

struct ParsedExecOutput {
    process_id: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
    output: String,
    structured: bool,
}

impl NonPaginatedExecHistory {
    pub(super) fn record_session_meta(&mut self, meta: &SessionMeta) {
        self.default_cwd = Some(LegacyAppPathString::from_path(&meta.cwd));
    }

    pub(super) fn record_turn_context(
        &mut self,
        context: &TurnContextItem,
        active_turn_id: Option<&str>,
    ) {
        let cwd = LegacyAppPathString::from_abs_path(&context.cwd);
        self.default_cwd = Some(cwd.clone());
        if let Some(turn_id) = context
            .turn_id
            .as_deref()
            .filter(|turn_id| !turn_id.is_empty())
            .or(active_turn_id)
        {
            self.turn_cwds.insert(turn_id.to_string(), cwd);
        }
    }

    pub(super) fn handle_response_item(
        &mut self,
        item: &ResponseItem,
    ) -> Option<NonPaginatedExecItemUpdate> {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } if name == "exec_command" => {
                self.handle_exec_command(arguments, call_id, item.turn_id())
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } if name == "write_stdin" => {
                self.handle_write_stdin(arguments, call_id);
                None
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => self.handle_function_call_output(call_id, output.body.to_text().as_deref()),
            _ => None,
        }
    }

    pub(super) fn remove_turns(&mut self, removed_turn_ids: &[String]) {
        self.turn_cwds
            .retain(|turn_id, _| !removed_turn_ids.contains(turn_id));
        self.commands
            .retain(|_, command| !command.belongs_to_any_turn(removed_turn_ids));
        self.process_commands
            .retain(|_, call_id| self.commands.contains_key(call_id));
        self.write_stdin_processes
            .retain(|_, process_id| self.process_commands.contains_key(process_id));
    }

    fn handle_exec_command(
        &mut self,
        arguments: &str,
        call_id: &str,
        turn_id: Option<&str>,
    ) -> Option<NonPaginatedExecItemUpdate> {
        let args: ExecCommandArgs = serde_json::from_str(arguments).ok()?;
        let turn_id = turn_id
            .filter(|turn_id| !turn_id.is_empty())
            .map(str::to_string);
        let cwd = args
            .workdir
            .map(LegacyAppPathString::from_string)
            .or_else(|| {
                turn_id
                    .as_ref()
                    .and_then(|turn_id| self.turn_cwds.get(turn_id).cloned())
            })
            .or_else(|| self.default_cwd.clone())?;
        let command = NonPaginatedExecCommand {
            call_id: call_id.to_string(),
            turn_id,
            command: args.cmd,
            cwd,
            process_id: None,
            status: CommandExecutionStatus::InProgress,
            aggregated_output: String::new(),
            exit_code: None,
            duration_ms: None,
        };
        let update = command.item_update();
        self.commands.insert(call_id.to_string(), command);
        Some(update)
    }

    fn handle_write_stdin(&mut self, arguments: &str, call_id: &str) {
        let Ok(args) = serde_json::from_str::<WriteStdinArgs>(arguments) else {
            return;
        };
        self.write_stdin_processes
            .insert(call_id.to_string(), args.session_id.to_string());
    }

    fn handle_function_call_output(
        &mut self,
        call_id: &str,
        raw_output: Option<&str>,
    ) -> Option<NonPaginatedExecItemUpdate> {
        let command_call_id = if self.commands.contains_key(call_id) {
            call_id.to_string()
        } else {
            let process_id = self.write_stdin_processes.remove(call_id)?;
            self.process_commands.get(&process_id)?.clone()
        };
        let command = self.commands.get_mut(&command_call_id)?;
        let parsed = parse_exec_output(raw_output.unwrap_or_default());

        if !parsed.output.is_empty() {
            command.aggregated_output.push_str(&parsed.output);
        }
        if let Some(duration_ms) = parsed.duration_ms {
            command.duration_ms = command
                .duration_ms
                .unwrap_or_default()
                .checked_add(duration_ms);
        }
        command.exit_code = parsed.exit_code;
        if let Some(process_id) = parsed.process_id {
            command.process_id = Some(process_id.clone());
            command.status = CommandExecutionStatus::InProgress;
            self.process_commands
                .insert(process_id, command_call_id.clone());
        } else if let Some(exit_code) = parsed.exit_code {
            command.status = if exit_code == 0 {
                CommandExecutionStatus::Completed
            } else {
                CommandExecutionStatus::Failed
            };
            if let Some(process_id) = command.process_id.as_ref() {
                self.process_commands.remove(process_id);
            }
        } else if parsed.structured {
            command.status = CommandExecutionStatus::Completed;
        } else {
            command.status = CommandExecutionStatus::Failed;
        }

        Some(command.item_update())
    }
}

impl NonPaginatedExecCommand {
    fn item_update(&self) -> NonPaginatedExecItemUpdate {
        NonPaginatedExecItemUpdate {
            turn_id: self.turn_id.clone(),
            item: ThreadItem::CommandExecution {
                id: self.call_id.clone(),
                plugin_id: None,
                script_path: None,
                command: self.command.clone(),
                cwd: self.cwd.clone(),
                process_id: self.process_id.clone(),
                source: CommandExecutionSource::UnifiedExecStartup,
                status: self.status.clone(),
                command_actions: vec![CommandAction::Unknown {
                    command: self.command.clone(),
                }],
                aggregated_output: (!self.aggregated_output.is_empty())
                    .then(|| self.aggregated_output.clone()),
                exit_code: self.exit_code,
                duration_ms: self.duration_ms,
            },
        }
    }

    fn belongs_to_any_turn(&self, turn_ids: &[String]) -> bool {
        self.turn_id
            .as_ref()
            .is_some_and(|turn_id| turn_ids.contains(turn_id))
    }
}

fn parse_exec_output(raw: &str) -> ParsedExecOutput {
    let raw = raw.trim_matches('\r');
    let Some(output_marker) = raw.find("\nOutput:") else {
        return ParsedExecOutput {
            process_id: None,
            exit_code: None,
            duration_ms: None,
            output: raw.to_string(),
            structured: false,
        };
    };
    let header = &raw[..output_marker];
    let output = &raw[output_marker + "\nOutput:".len()..];
    let output = output.strip_prefix('\n').unwrap_or(output).to_string();
    let mut process_id = None;
    let mut exit_code = None;
    let mut duration_ms = None;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("Process running with session ID ") {
            process_id = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("Process exited with code ") {
            exit_code = value.parse().ok();
        } else if let Some(value) = line
            .strip_prefix("Wall time: ")
            .and_then(|value| value.strip_suffix(" seconds"))
            && let Ok(seconds) = value.parse::<f64>()
            && seconds.is_finite()
            && seconds >= 0.0
        {
            duration_ms = Duration::try_from_secs_f64(seconds)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_millis()).ok());
        }
    }
    ParsedExecOutput {
        process_id,
        exit_code,
        duration_ms,
        output,
        structured: true,
    }
}

#[cfg(test)]
#[path = "non_paginated_exec_tests.rs"]
mod tests;
