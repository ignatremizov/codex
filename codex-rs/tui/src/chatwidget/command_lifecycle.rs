//! Command execution lifecycle handlers for `ChatWidget`.
//!
//! This module owns command start/output/completion rendering, including active
//! exec-cell grouping and unified exec process state.

use super::*;
use crate::bottom_pane::BackgroundTerminalCompletion;
use crate::exec_cell::CommandOutput;

impl ChatWidget {
    pub(super) fn on_command_execution_started(
        &mut self,
        item: ThreadItem,
        deadline_at_ms: Option<i64>,
        turn_id: &str,
    ) {
        let ThreadItem::CommandExecution {
            id,
            command,
            process_id,
            source,
            user_shell_response_handling,
            command_actions,
            ..
        } = &item
        else {
            return;
        };
        let (_command, parsed_cmd) = command_execution_command_and_parsed(command, command_actions);
        self.flush_answer_stream_with_separator();
        if *source == ExecCommandSource::UserShell && process_id.is_some() {
            self.track_unified_exec_process_begin(
                id,
                process_id.as_deref(),
                command,
                *user_shell_response_handling,
            );
        }
        if is_unified_exec_source(*source) {
            if *source == ExecCommandSource::UnifiedExecStartup {
                self.track_unified_exec_process_begin(
                    id,
                    process_id.as_deref(),
                    command,
                    /*user_shell_response_handling*/ None,
                );
            }
            if !self.bottom_pane.is_task_running() {
                return;
            }
            // Unified exec may be parsed as Unknown; keep the working indicator visible regardless.
            self.bottom_pane.ensure_status_indicator();
            if *source == ExecCommandSource::UnifiedExecStartup {
                if self
                    .unified_exec_wait_tracker
                    .as_ref()
                    .is_some_and(UnifiedExecWaitTracker::has_active_process_waits)
                {
                    self.refresh_unified_exec_wait_status();
                } else {
                    let process_key = process_id.as_deref().unwrap_or(id);
                    let command_display = self
                        .unified_exec_processes
                        .iter()
                        .find(|process| process.key == process_key)
                        .map(|process| process.command_display.clone());
                    self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
                    self.set_status(
                        "Working".to_string(),
                        command_display,
                        StatusDetailsCapitalization::Preserve,
                        /*details_max_lines*/ 1,
                    );
                }
            }
            if let Some(deadline_at_ms) = deadline_at_ms {
                self.set_status_countdown_deadline_at_ms(
                    StatusCountdownOwner::UnifiedExec {
                        turn_id: turn_id.to_string(),
                        item_id: id.clone(),
                        process_id: process_id.as_deref().unwrap_or(id).to_string(),
                    },
                    deadline_at_ms,
                );
            }
            if !is_standard_tool_call(&parsed_cmd) {
                return;
            }
        }
        self.defer_or_handle(
            item,
            InterruptManager::push_item_started,
            Self::handle_command_execution_started_now,
        );
    }

    pub(super) fn on_exec_command_output_delta(
        &mut self,
        call_id: &str,
        delta: &str,
        replay_kind: Option<ReplayKind>,
    ) {
        self.track_unified_exec_output_chunk(call_id, delta.as_bytes());
        if replay_kind.is_none() && !self.bottom_pane.is_task_running() {
            return;
        }

        let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
        else {
            return;
        };

        if cell.append_output(call_id, delta) {
            self.bump_active_cell_revision();
            self.request_redraw();
        }
    }

    pub(super) fn on_command_execution_completed(&mut self, item: ThreadItem, turn_id: &str) {
        let ThreadItem::CommandExecution {
            id,
            process_id,
            source,
            ..
        } = &item
        else {
            return;
        };
        if *source == ExecCommandSource::UserShell && process_id.is_some() {
            self.track_unified_exec_process_end(id, process_id.as_deref());
        }
        if is_unified_exec_source(*source) {
            let owner = StatusCountdownOwner::UnifiedExec {
                turn_id: turn_id.to_string(),
                item_id: id.clone(),
                process_id: process_id.as_deref().unwrap_or(id).to_string(),
            };
            self.clear_status_countdown_if_owner(&owner);
            let matching_process = self.unified_exec_processes.iter().any(|process| {
                process.key == process_id.as_deref().unwrap_or(id) && process.call_id == *id
            });
            let completed_wait_streak = process_id.as_deref().is_some_and(|process_id| {
                matching_process
                    && self
                        .unified_exec_wait_tracker
                        .as_ref()
                        .is_some_and(|tracker| {
                            tracker.has_transcript_streak_for_process(process_id)
                        })
            });
            if completed_wait_streak {
                // Flushing historical detail must not reset another wait's live header.
                self.flush_unified_exec_wait_streak();
            }
            self.track_unified_exec_process_end(id, process_id.as_deref());
            if !self.bottom_pane.is_task_running() {
                return;
            }
        }
        self.defer_or_handle(
            item,
            InterruptManager::push_item_completed,
            Self::handle_command_execution_completed_now,
        );
    }

    pub(super) fn track_unified_exec_process_begin(
        &mut self,
        call_id: &str,
        process_id: Option<&str>,
        command: &str,
        user_shell_response_handling: Option<
            codex_app_server_protocol::ThreadShellCommandResponseHandling,
        >,
    ) {
        let key = process_id.unwrap_or(call_id).to_string();
        self.completed_unified_exec_processes
            .retain(|process| process.key != key);
        let command = split_command_string(command);
        let command_display = strip_bash_lc_and_escape(&command);
        if let Some(existing) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.key == key)
        {
            existing.call_id = call_id.to_string();
            existing.command_display = command_display;
            existing.recent_chunks = crate::exec_cell::LiveCommandOutput::default();
            existing.user_shell_response_handling = user_shell_response_handling;
        } else {
            self.unified_exec_processes.push(UnifiedExecProcessSummary {
                key,
                call_id: call_id.to_string(),
                command_display,
                recent_chunks: crate::exec_cell::LiveCommandOutput::default(),
                user_shell_response_handling,
            });
        }
        self.sync_unified_exec_footer();
    }

    pub(super) fn sync_unified_exec_footer(&mut self) {
        let processes = self
            .unified_exec_processes
            .iter()
            .map(|process| BackgroundTerminalCompletion {
                process_id: process.key.clone(),
                command_display: process.command_display.clone(),
            })
            .collect();
        self.bottom_pane.set_unified_exec_processes(processes);
    }

    /// Record recent stdout/stderr lines for the unified exec footer.
    pub(super) fn track_unified_exec_output_chunk(&mut self, call_id: &str, chunk: &[u8]) {
        let Some(process) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.call_id == call_id)
        else {
            return;
        };

        process
            .recent_chunks
            .push_str(&String::from_utf8_lossy(chunk));
    }

    pub(crate) fn handle_command_execution_started_now(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            command,
            source,
            command_actions,
            user_shell_response_handling,
            ..
        } = item
        else {
            return;
        };
        let (command, parsed_cmd) =
            command_execution_command_and_parsed(&command, &command_actions);
        // Detached user-shell commands use the background-terminal footer and
        // must not make the composer look like a model turn is running.
        if self.bottom_pane.is_task_running() {
            self.bottom_pane.ensure_status_indicator();
        }
        let parsed_cmd = self.annotate_skill_reads_in_parsed_cmd(parsed_cmd);
        self.running_commands.insert(
            id.clone(),
            RunningCommand {
                command: command.clone(),
                parsed_cmd: parsed_cmd.clone(),
                source,
                user_shell_response_handling,
            },
        );
        let is_wait_interaction = matches!(source, ExecCommandSource::UnifiedExecInteraction);
        let command_display = command.join(" ");
        let should_suppress_unified_wait = is_wait_interaction
            && self
                .last_unified_wait
                .as_ref()
                .is_some_and(|wait| wait.is_duplicate(&command_display));
        if is_wait_interaction {
            self.last_unified_wait = Some(UnifiedExecWaitState::new(command_display));
        } else {
            self.last_unified_wait = None;
        }
        if should_suppress_unified_wait {
            self.suppressed_exec_calls.insert(id);
            return;
        }
        if let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
            && cell.add_call(
                id.clone(),
                command.clone(),
                parsed_cmd.clone(),
                source,
                user_shell_response_handling,
                /*interaction_input*/ None,
            )
        {
            self.bump_active_cell_revision();
        } else {
            self.flush_active_cell();

            self.transcript.active_cell = Some(Box::new(
                new_active_exec_command(
                    crate::exec_cell::ActiveExecCall {
                        call_id: id,
                        command,
                        parsed: parsed_cmd,
                        source,
                        user_shell_response_handling,
                        interaction_input: None,
                    },
                    self.config.animations,
                )
                .with_output_preview_line_limits(OutputPreviewLineLimits {
                    command: self.config.tui_command_output_preview_lines,
                    user_shell: self.config.tui_user_shell_output_preview_lines,
                }),
            ));
            self.bump_active_cell_revision();
        }

        self.request_redraw();
    }

    /// Finalizes an exec call while preserving the active exec cell grouping contract.
    ///
    /// Exec begin/end events usually pair through `running_commands`, but unified exec can emit an
    /// end event for a call that was never materialized as the current active `ExecCell` (for
    /// example, when another exploring group is still active). In that case we render the end as a
    /// standalone history entry instead of replacing or flushing the unrelated active exploring
    /// cell. If this method treated every unknown end as "complete the active cell", the UI could
    /// merge unrelated commands and hide still-running exploring work.
    pub(crate) fn handle_command_execution_completed_now(&mut self, item: ThreadItem) {
        enum ExecEndTarget {
            // Normal case: the active exec cell already tracks this call id.
            ActiveTracked,
            // We have an active exec group, but it does not contain this call id. Render the end
            // as a standalone finalized history cell so the active group remains intact.
            OrphanHistoryWhileActiveExec,
            // No active exec cell can safely own this end; build a new cell from the end payload.
            NewCell,
        }

        let ThreadItem::CommandExecution {
            id,
            command,
            process_id: _,
            source,
            user_shell_response_handling,
            status,
            command_actions,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } = item
        else {
            return;
        };
        let event_command = split_command_string(&command);
        let event_parsed = command_actions
            .into_iter()
            .map(codex_app_server_protocol::CommandAction::into_core)
            .collect();
        let duration = Duration::from_millis(duration_ms.unwrap_or_default().max(0) as u64);
        let exit_code = if status == codex_app_server_protocol::CommandExecutionStatus::Completed {
            exit_code.unwrap_or_default()
        } else {
            exit_code.filter(|code| *code != 0).unwrap_or(1)
        };
        let aggregated_output = aggregated_output.unwrap_or_default();

        let running = self.running_commands.remove(&id);
        if self.suppressed_exec_calls.remove(&id) {
            return;
        }
        let (command, parsed, source, user_shell_response_handling) = match running {
            Some(rc) => (
                rc.command,
                rc.parsed_cmd,
                rc.source,
                rc.user_shell_response_handling,
            ),
            None => (
                event_command,
                event_parsed,
                source,
                user_shell_response_handling,
            ),
        };
        let parsed = self.annotate_skill_reads_in_parsed_cmd(parsed);
        let is_unified_exec_interaction =
            matches!(source, ExecCommandSource::UnifiedExecInteraction);
        let is_user_shell = source == ExecCommandSource::UserShell;
        // Completion-only replay has no begin event to join adjacent exploration. Extend only
        // a finished, compatible group; an unrelated running group must retain orphan routing.
        if let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<ExecCell>())
            && !cell.is_active()
            && !cell.should_flush()
            && !cell.iter_calls().any(|call| call.call_id == id)
        {
            cell.add_call(
                id.clone(),
                command.clone(),
                parsed.clone(),
                source,
                user_shell_response_handling,
                /*interaction_input*/ None,
            );
        }
        let end_target = match self.transcript.active_cell.as_ref() {
            Some(cell) => match cell.as_any().downcast_ref::<ExecCell>() {
                Some(exec_cell) if exec_cell.iter_calls().any(|call| call.call_id == id) => {
                    ExecEndTarget::ActiveTracked
                }
                Some(exec_cell) if exec_cell.is_active() => {
                    ExecEndTarget::OrphanHistoryWhileActiveExec
                }
                None if cell.as_any().is::<McpToolCallCell>()
                    || cell
                        .as_any()
                        .downcast_ref::<history_cell::ComputerActivityCell>()
                        .is_some_and(history_cell::ComputerActivityCell::is_active) =>
                {
                    ExecEndTarget::OrphanHistoryWhileActiveExec
                }
                Some(_) | None => ExecEndTarget::NewCell,
            },
            None => ExecEndTarget::NewCell,
        };

        let output = if is_unified_exec_interaction {
            CommandOutput::new(exit_code, String::new())
        } else {
            CommandOutput::new(exit_code, aggregated_output)
        };

        match end_target {
            ExecEndTarget::ActiveTracked => {
                let has_active_hook = self
                    .active_hook_cell
                    .as_ref()
                    .is_some_and(HookCell::has_visible_running_run);
                if let Some(cell) = self
                    .transcript
                    .active_cell
                    .as_mut()
                    .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
                {
                    let completed = cell.complete_call(&id, output, duration);
                    debug_assert!(completed, "active exec cell should contain {id}");
                    if cell.should_flush() || (has_active_hook && !cell.is_active()) {
                        self.flush_active_cell();
                    } else {
                        self.bump_active_cell_revision();
                        self.request_redraw();
                    }
                }
            }
            ExecEndTarget::OrphanHistoryWhileActiveExec => {
                let mut orphan = new_active_exec_command(
                    crate::exec_cell::ActiveExecCall {
                        call_id: id.clone(),
                        command,
                        parsed,
                        source,
                        user_shell_response_handling,
                        interaction_input: None,
                    },
                    self.local_settings.tui.animations && self.local_settings.tui.effects.progress,
                )
                .with_output_preview_line_limits(OutputPreviewLineLimits {
                    command: self.local_settings.tui.command_output_preview_lines,
                    user_shell: self.local_settings.tui.user_shell_output_preview_lines,
                });
                let completed = orphan.complete_call(&id, output, duration);
                debug_assert!(completed, "new orphan exec cell should contain {id}");
                self.app_event_tx
                    .send(AppEvent::InsertHistoryCell(Box::new(orphan)));
                self.request_redraw();
            }
            ExecEndTarget::NewCell => {
                let mut cell = new_active_exec_command(
                    crate::exec_cell::ActiveExecCall {
                        call_id: id.clone(),
                        command,
                        parsed,
                        source,
                        user_shell_response_handling,
                        interaction_input: None,
                    },
                    self.local_settings.tui.animations && self.local_settings.tui.effects.progress,
                )
                .with_output_preview_line_limits(OutputPreviewLineLimits {
                    command: self.local_settings.tui.command_output_preview_lines,
                    user_shell: self.local_settings.tui.user_shell_output_preview_lines,
                });
                let completed = cell.complete_call(&id, output, duration);
                debug_assert!(completed, "new exec cell should contain {id}");
                if let Some(active) = self
                    .transcript
                    .active_cell
                    .as_mut()
                    .and_then(|cell| cell.as_any_mut().downcast_mut::<ExecCell>())
                    && !active.is_active()
                    && active.is_exploring_cell()
                    && cell.is_exploring_cell()
                {
                    // Replayed commands have completion events without matching starts.
                    active.group.calls.extend(cell.group.calls);
                    self.bump_active_cell_revision();
                    self.request_redraw();
                } else {
                    self.flush_active_cell();
                    if cell.should_flush() {
                        self.add_to_history(cell);
                    } else {
                        self.transcript.active_cell = Some(Box::new(cell));
                        self.bump_active_cell_revision();
                        self.request_redraw();
                    }
                }
            }
        }
        if is_user_shell {
            self.maybe_send_next_queued_input();
        }
    }
}
