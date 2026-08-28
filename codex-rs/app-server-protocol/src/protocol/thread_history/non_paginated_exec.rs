use crate::protocol::item_builders::CommandExecutionPresentation;
use crate::protocol::v2::CommandExecutionSource;
use crate::protocol::v2::CommandExecutionStatus;
use crate::protocol::v2::ThreadItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

/// Projects only sources identified as Legacy by their first session header.
/// Headerless inputs and raw context preceding a delayed header are not replayed.
///
/// The journal contains only command contributions and cwd changes, not a copy
/// of the rollout. Ordinals let rollback undo a later poll's changes to an older
/// command without changing the parent's materialized turn boundaries.
#[derive(Default)]
pub(super) struct NonPaginatedExecHistory {
    mode: Option<ThreadHistoryMode>,
    journal: Vec<(usize, Contribution)>,
    state: ExecState,
    authoritative: HashMap<String, String>,
}

#[derive(Default)]
struct ExecState {
    default_cwd: Option<PathUri>,
    turn_cwds: HashMap<String, PathUri>,
    commands: HashMap<String, ExecCommand>,
    pending_initial_outputs: HashSet<String>,
    process_commands: HashMap<String, String>,
    poll_commands: HashMap<String, String>,
}

#[derive(Clone)]
enum Contribution {
    Cwd {
        turn_id: Option<String>,
        cwd: PathUri,
    },
    Start(ExecCommand),
    Poll {
        call_id: String,
        command_call_id: String,
    },
    Output {
        call_id: String,
        output: ParsedExecOutput,
    },
}

pub(super) struct NonPaginatedExecItemUpdate {
    pub(super) turn_id: String,
    pub(super) item: ThreadItem,
}

#[derive(Clone, PartialEq)]
struct ExecCommand {
    call_id: String,
    turn_id: String,
    command: String,
    cwd: PathUri,
    process_id: Option<String>,
    status: CommandExecutionStatus,
    output: String,
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

#[derive(Clone)]
struct ParsedExecOutput {
    process_id: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
    output: String,
}

impl NonPaginatedExecHistory {
    pub(super) fn record_session_meta(&mut self, meta: &SessionMeta, ordinal: usize) {
        if self.mode.is_some() {
            return;
        }
        self.mode = Some(meta.history_mode);
        if matches!(self.mode, Some(ThreadHistoryMode::Legacy))
            && let Some(cwd) = LegacyAppPathString::from_path(&meta.cwd).to_inferred_path_uri()
        {
            self.record(ordinal, Contribution::Cwd { turn_id: None, cwd });
        }
    }

    pub(super) fn record_turn_context(&mut self, context: &TurnContextItem, ordinal: usize) {
        if matches!(self.mode, Some(ThreadHistoryMode::Legacy))
            && let Some(cwd) =
                LegacyAppPathString::from_abs_path(&context.cwd).to_inferred_path_uri()
        {
            self.record(
                ordinal,
                Contribution::Cwd {
                    turn_id: context.turn_id.clone(),
                    cwd,
                },
            );
        }
    }

    pub(super) fn mark_authoritative(&mut self, call_id: &str, turn_id: &str) {
        self.authoritative
            .insert(call_id.to_string(), turn_id.to_string());
    }

    pub(super) fn handle_response_item(
        &mut self,
        item: &ResponseItem,
        ordinal: usize,
        fallback_turn_id: &str,
    ) -> Option<NonPaginatedExecItemUpdate> {
        if !matches!(self.mode, Some(ThreadHistoryMode::Legacy)) {
            return None;
        }
        let contribution = match item {
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } if namespace
                .as_deref()
                .is_none_or(|namespace| namespace == "functions")
                && name == "exec_command" =>
            {
                if self.state.commands.contains_key(call_id) {
                    return None;
                }
                let args: ExecCommandArgs = serde_json::from_str(arguments).ok()?;
                let turn_id = item.turn_id().unwrap_or(fallback_turn_id).to_string();
                let base = self
                    .state
                    .turn_cwds
                    .get(&turn_id)
                    .or(self.state.default_cwd.as_ref());
                let cwd = match args.workdir {
                    Some(workdir) => {
                        let path = LegacyAppPathString::from_string(&workdir);
                        PathUri::parse(&workdir)
                            .ok()
                            .or_else(|| path.to_inferred_path_uri())
                            .or_else(|| {
                                base.and_then(|cwd| {
                                    path.resolve_against(cwd, /*user_home_dir*/ None).ok()
                                })
                            })?
                    }
                    None => base.cloned()?,
                };
                Contribution::Start(ExecCommand {
                    call_id: call_id.clone(),
                    turn_id,
                    command: args.cmd,
                    cwd,
                    process_id: None,
                    status: CommandExecutionStatus::InProgress,
                    output: String::new(),
                    exit_code: None,
                    duration_ms: None,
                })
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } if namespace
                .as_deref()
                .is_none_or(|namespace| namespace == "functions")
                && name == "write_stdin" =>
            {
                let args: WriteStdinArgs = serde_json::from_str(arguments).ok()?;
                // Bind at invocation, not when output arrives: process IDs can be reused.
                let command_call_id = self
                    .state
                    .process_commands
                    .get(&args.session_id.to_string())?
                    .clone();
                Contribution::Poll {
                    call_id: call_id.clone(),
                    command_call_id,
                }
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                output,
                ..
            } => {
                if !self.state.pending_initial_outputs.contains(call_id)
                    && !self.state.poll_commands.contains_key(call_id)
                {
                    return None;
                }
                let raw = output.body.to_text()?;
                Contribution::Output {
                    call_id: call_id.clone(),
                    output: parse_exec_output(&raw),
                }
            }
            _ => return None,
        };
        let changed = self.record(ordinal, contribution)?;
        self.update(&changed)
    }

    pub(super) fn rollback(
        &mut self,
        cutoff: usize,
        removed_turn_ids: &[String],
    ) -> Vec<NonPaginatedExecItemUpdate> {
        self.authoritative
            .retain(|_, turn_id| !removed_turn_ids.contains(turn_id));
        let journal = std::mem::take(&mut self.journal);
        let previous = std::mem::take(&mut self.state);
        for (ordinal, contribution) in journal {
            let removed_origin = match &contribution {
                Contribution::Cwd { turn_id, .. } => turn_id
                    .as_ref()
                    .is_some_and(|turn_id| removed_turn_ids.contains(turn_id)),
                Contribution::Start(command) => removed_turn_ids.contains(&command.turn_id),
                Contribution::Poll { .. } | Contribution::Output { .. } => false,
            };
            if ordinal < cutoff && !removed_origin {
                self.record(ordinal, contribution);
            }
        }
        self.journal
            .iter()
            .filter_map(|(_, contribution)| {
                let Contribution::Start(command) = contribution else {
                    return None;
                };
                if previous.commands.get(&command.call_id)
                    == self.state.commands.get(&command.call_id)
                {
                    return None;
                }
                self.update(&command.call_id)
            })
            .collect()
    }

    fn record(&mut self, ordinal: usize, contribution: Contribution) -> Option<String> {
        let changed = self.state.apply(&contribution);
        self.journal.push((ordinal, contribution));
        changed
    }

    fn update(&self, call_id: &str) -> Option<NonPaginatedExecItemUpdate> {
        if self.authoritative.contains_key(call_id) {
            return None;
        }
        let command = self.state.commands.get(call_id)?;
        let presentation = CommandExecutionPresentation::from_raw(
            std::slice::from_ref(&command.command),
            &[ParsedCommand::Unknown {
                cmd: command.command.clone(),
            }],
            &command.cwd,
        );
        Some(NonPaginatedExecItemUpdate {
            turn_id: command.turn_id.clone(),
            item: ThreadItem::CommandExecution {
                model_context: None,
                id: command.call_id.clone(),
                plugin_id: None,
                script_path: None,
                command: presentation.command,
                cwd: command.cwd.clone().into(),
                process_id: command.process_id.clone(),
                source: CommandExecutionSource::UnifiedExecStartup,
                user_shell_response_handling: None,
                status: command.status.clone(),
                command_actions: presentation.command_actions,
                aggregated_output: (!command.output.is_empty()).then(|| command.output.clone()),
                exit_code: command.exit_code,
                duration_ms: command.duration_ms,
            },
        })
    }
}

impl ExecState {
    fn apply(&mut self, contribution: &Contribution) -> Option<String> {
        match contribution {
            Contribution::Cwd { turn_id, cwd } => {
                self.default_cwd = Some(cwd.clone());
                if let Some(turn_id) = turn_id {
                    self.turn_cwds.insert(turn_id.clone(), cwd.clone());
                }
                None
            }
            Contribution::Start(command) => {
                self.commands
                    .insert(command.call_id.clone(), command.clone());
                self.pending_initial_outputs.insert(command.call_id.clone());
                Some(command.call_id.clone())
            }
            Contribution::Poll {
                call_id,
                command_call_id,
            } => {
                self.poll_commands
                    .insert(call_id.clone(), command_call_id.clone());
                None
            }
            Contribution::Output { call_id, output } => {
                let command_call_id = if self.pending_initial_outputs.remove(call_id) {
                    call_id.clone()
                } else {
                    self.poll_commands.remove(call_id)?
                };
                let command = self.commands.get_mut(&command_call_id)?;
                command.output.push_str(&output.output);
                if let Some(duration_ms) = output.duration_ms {
                    command.duration_ms = command
                        .duration_ms
                        .unwrap_or_default()
                        .checked_add(duration_ms);
                }
                if command.exit_code.is_some() && output.process_id.is_some() {
                    return Some(command_call_id);
                }
                command.exit_code = output.exit_code;
                if let Some(process_id) = &output.process_id {
                    command.process_id = Some(process_id.clone());
                    command.status = CommandExecutionStatus::InProgress;
                    if call_id == &command_call_id {
                        self.process_commands
                            .insert(process_id.clone(), command_call_id.clone());
                    } else {
                        self.process_commands
                            .entry(process_id.clone())
                            .or_insert_with(|| command_call_id.clone());
                    }
                } else {
                    command.status = if output.exit_code == Some(0) {
                        CommandExecutionStatus::Completed
                    } else {
                        CommandExecutionStatus::Failed
                    };
                    if let Some(process_id) = &command.process_id
                        && self.process_commands.get(process_id) == Some(&command_call_id)
                    {
                        self.process_commands.remove(process_id);
                    }
                }
                Some(command_call_id)
            }
        }
    }
}

fn parse_exec_output(raw: &str) -> ParsedExecOutput {
    let mut parsed = ParsedExecOutput {
        process_id: None,
        exit_code: None,
        duration_ms: None,
        output: raw.to_string(),
    };
    let Some((header, output)) = raw.split_once("\nOutput:") else {
        return parsed;
    };
    // A random tool error containing "Output:" is not evidence of completion.
    if !header.starts_with("Chunk ID: ") {
        return parsed;
    }
    parsed.output = output.strip_prefix('\n').unwrap_or(output).to_string();
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("Process running with session ID ")
            && let Ok(process_id) = value.parse::<i32>()
        {
            parsed.process_id = Some(process_id.to_string());
        } else if let Some(value) = line.strip_prefix("Process exited with code ") {
            parsed.exit_code = value.parse().ok();
        } else if let Some(value) = line
            .strip_prefix("Wall time: ")
            .and_then(|value| value.strip_suffix(" seconds"))
            && let Ok(seconds) = value.parse::<f64>()
        {
            parsed.duration_ms = Duration::try_from_secs_f64(seconds)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_millis()).ok());
        }
    }
    if parsed.process_id.is_some() && parsed.exit_code.is_some() {
        parsed.process_id = None;
        parsed.exit_code = None;
    }
    parsed
}

#[cfg(test)]
#[path = "non_paginated_exec_tests.rs"]
mod tests;
