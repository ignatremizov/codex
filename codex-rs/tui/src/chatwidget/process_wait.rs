//! Per-invocation unified-exec wait state and status presentation.

use super::exec_state::UnifiedExecWaitStreak;
use super::*;
use crate::status_indicator_widget::fmt_elapsed_compact;
use codex_app_server_protocol::TerminalInteractionNotification;
use codex_app_server_protocol::TerminalWait;
use codex_app_server_protocol::TerminalWaitMode;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const MAX_VISIBLE_PROCESS_WAITS: usize = 3;

#[derive(Debug, Default)]
pub(super) struct UnifiedExecWaitTracker {
    active: HashMap<UnifiedExecWaitKey, ActiveTerminalWait>,
    transcript_streak: Option<UnifiedExecWaitStreak>,
}

impl UnifiedExecWaitTracker {
    pub(super) fn has_active_wait(&self) -> bool {
        !self.active.is_empty() || self.transcript_streak.is_some()
    }

    pub(super) fn has_active_process_waits(&self) -> bool {
        !self.active.is_empty()
    }

    pub(super) fn has_transcript_streak_for_process(&self, process_id: &str) -> bool {
        self.transcript_streak
            .as_ref()
            .is_some_and(|wait| wait.process_id == process_id)
    }

    fn is_empty(&self) -> bool {
        self.active.is_empty() && self.transcript_streak.is_none()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct UnifiedExecWaitKey {
    turn_id: String,
    interaction_id: String,
}

#[derive(Debug)]
struct ActiveTerminalWait {
    call_id: String,
    process_id: String,
    command_display: Option<String>,
    started_at_ms: i64,
    deadline_at_ms: Option<i64>,
    mode: TerminalWaitMode,
}

impl ChatWidget {
    pub(super) fn flush_unified_exec_wait_streak(&mut self) {
        let wait = self
            .unified_exec_wait_tracker
            .as_mut()
            .and_then(|tracker| tracker.transcript_streak.take());
        let Some(wait) = wait else {
            return;
        };
        let cell = history_cell::new_unified_exec_interaction(wait.command_display, String::new());
        self.on_async_agent_notice(cell);
        self.restore_reasoning_status_header();
        self.discard_empty_unified_exec_wait_tracker();
    }

    pub(super) fn on_terminal_interaction(
        &mut self,
        notification: TerminalInteractionNotification,
    ) {
        if let Some(wait) = notification.wait.clone() {
            self.on_terminal_wait(notification, wait);
            return;
        }
        if !self.bottom_pane.is_task_running() {
            return;
        }

        let TerminalInteractionNotification {
            turn_id,
            item_id,
            process_id,
            stdin,
            deadline_at_ms,
            ..
        } = notification;
        let active_command_display = self
            .unified_exec_processes
            .iter()
            .find(|process| process.key == process_id || process.call_id == item_id)
            .map(|process| process.command_display.clone())
            .filter(|command| !command.is_empty());
        if stdin.is_empty() && active_command_display.is_none() && deadline_at_ms.is_some() {
            return;
        }
        if stdin.is_empty() && active_command_display.is_none() {
            // Empty polls start with a deadline and finish without one; render the completion
            // half if the process has already left the active map.
            let command_display = self
                .completed_unified_exec_processes
                .iter()
                .rev()
                .find(|process| process.key == process_id)
                .map(|process| process.command_display.clone());
            self.flush_answer_stream_with_separator();
            self.add_to_history(history_cell::new_unified_exec_output_check(command_display));
            return;
        }
        let command_display = active_command_display;

        self.flush_answer_stream_with_separator();
        if stdin.is_empty() {
            // Keep empty-stdin background polls in the status row and preserve legacy streak grouping.
            self.bottom_pane.ensure_status_indicator();
            self.bottom_pane
                .set_interrupt_hint_visible(/*visible*/ true);
            self.status_state.terminal_title_status_kind =
                TerminalTitleStatusKind::WaitingForBackgroundTerminal;
            self.set_status(
                "Waiting for background terminal".to_string(),
                command_display.clone(),
                StatusDetailsCapitalization::Preserve,
                /*details_max_lines*/ 1,
            );
            let countdown_owner = StatusCountdownOwner::UnifiedExec {
                turn_id: turn_id.clone(),
                item_id: item_id.clone(),
                process_id: process_id.clone(),
            };
            if let Some(deadline_at_ms) = deadline_at_ms {
                self.set_status_countdown_deadline_at_ms(countdown_owner, deadline_at_ms);
            } else {
                self.clear_status_countdown_if_owner(&countdown_owner);
            }

            let same_process_streak =
                self.unified_exec_wait_tracker
                    .as_ref()
                    .is_some_and(|tracker| {
                        tracker
                            .transcript_streak
                            .as_ref()
                            .is_some_and(|wait| wait.process_id == process_id)
                    });
            if same_process_streak
                && let Some(wait) = self
                    .unified_exec_wait_tracker
                    .as_mut()
                    .and_then(|tracker| tracker.transcript_streak.as_mut())
            {
                wait.update_command_display(command_display);
            } else {
                self.flush_unified_exec_wait_streak();
                self.unified_exec_wait_tracker
                    .get_or_insert_with(UnifiedExecWaitTracker::default)
                    .transcript_streak =
                    Some(UnifiedExecWaitStreak::new(process_id, command_display));
            }
            self.request_redraw();
        } else {
            if self
                .unified_exec_wait_tracker
                .as_ref()
                .is_some_and(|tracker| tracker.has_transcript_streak_for_process(&process_id))
            {
                self.flush_unified_exec_wait_streak();
                self.clear_status_countdown_if_owner(&StatusCountdownOwner::UnifiedExec {
                    turn_id,
                    item_id,
                    process_id,
                });
            }
            self.add_to_history(history_cell::new_unified_exec_interaction(
                command_display,
                stdin,
            ));
        }
    }

    pub(super) fn refresh_unified_exec_wait_status(&mut self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or_default();
        self.refresh_unified_exec_wait_status_at(now_ms);
    }

    pub(super) fn refresh_unified_exec_wait_status_at(&mut self, now_ms: i64) {
        let Some((header, details, details_max_lines)) =
            self.process_wait_status_content_at(now_ms)
        else {
            self.clear_unified_exec_wait_invocation_countdown();
            if is_unified_exec_process_wait_header(&self.status_state.current_status.header) {
                self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
                self.set_status_header(String::from("Working"));
            }
            return;
        };
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane
            .set_interrupt_hint_visible(/*visible*/ true);
        self.status_state.terminal_title_status_kind =
            TerminalTitleStatusKind::WaitingForBackgroundTerminal;
        if self.status_state.current_status.header != header
            || self.status_state.current_status.details.as_deref() != Some(details.as_str())
        {
            self.set_status(
                header,
                Some(details),
                StatusDetailsCapitalization::Preserve,
                details_max_lines,
            );
        }
        self.sync_unified_exec_wait_countdown();
        if !self.config.animations {
            self.frame_requester
                .schedule_frame_in(Duration::from_secs(1));
        }
    }

    pub(super) fn process_wait_status_content_at(
        &self,
        now_ms: i64,
    ) -> Option<(String, String, usize)> {
        let tracker = self.unified_exec_wait_tracker.as_ref()?;
        if tracker.active.is_empty() {
            return None;
        }

        let mut active_waits = tracker.active.iter().collect::<Vec<_>>();
        active_waits.sort_by(|(left_key, left), (right_key, right)| {
            right
                .started_at_ms
                .cmp(&left.started_at_ms)
                .then_with(|| left_key.interaction_id.cmp(&right_key.interaction_id))
                .then_with(|| left_key.turn_id.cmp(&right_key.turn_id))
        });
        let process_count = active_waits
            .iter()
            .map(|(_, wait)| wait.process_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let header = match active_waits.as_slice() {
            [(_, wait)] if wait.mode == TerminalWaitMode::UntilExit => {
                String::from("Waiting for process exit")
            }
            [(_, _)] => String::from("Waiting for timed process result"),
            _ if process_count == 1 => String::from("Waiting on 1 process"),
            _ => format!("Waiting on {process_count} processes"),
        };
        let mut details = active_waits
            .iter()
            .take(MAX_VISIBLE_PROCESS_WAITS)
            .map(|(key, wait)| {
                let elapsed_ms = u64::try_from(now_ms.saturating_sub(wait.started_at_ms).max(0))
                    .unwrap_or_default();
                let is_single_timed_wait = active_waits.len() == 1
                    && wait.mode == TerminalWaitMode::Timed
                    && wait.deadline_at_ms.is_some();
                let wait_time = match wait.mode {
                    TerminalWaitMode::UntilExit => {
                        format!("{} elapsed", fmt_elapsed_compact(elapsed_ms / 1_000))
                    }
                    TerminalWaitMode::Timed => match wait.deadline_at_ms {
                        Some(deadline_at_ms) => {
                            let remaining_ms =
                                u64::try_from(deadline_at_ms.saturating_sub(now_ms).max(0))
                                    .unwrap_or_default();
                            let remaining_seconds =
                                remaining_ms / 1_000 + u64::from(remaining_ms % 1_000 > 0);
                            format!("{} left", fmt_elapsed_compact(remaining_seconds))
                        }
                        None => format!("{} elapsed", fmt_elapsed_compact(elapsed_ms / 1_000)),
                    },
                };
                let wait_mode = match wait.mode {
                    TerminalWaitMode::Timed => "Timed",
                    TerminalWaitMode::UntilExit => "Until exit",
                };
                let command_display = wait
                    .command_display
                    .clone()
                    .or_else(|| {
                        self.unified_exec_process_command_display(&wait.call_id, &wait.process_id)
                    })
                    .filter(|command| !command.is_empty())
                    .unwrap_or_else(|| String::from("background terminal"));
                let time = (!is_single_timed_wait).then(|| format!("{wait_time} · "));
                format!(
                    "{wait_mode} · {}{command_display} · process {} · wait {}",
                    time.unwrap_or_default(),
                    wait.process_id,
                    key.interaction_id
                )
            })
            .collect::<Vec<_>>();
        let remaining = active_waits.len().saturating_sub(MAX_VISIBLE_PROCESS_WAITS);
        if remaining > 0 {
            details.push(format!("+{remaining} more"));
        }
        Some((header, details.join("\n"), MAX_VISIBLE_PROCESS_WAITS + 1))
    }

    pub(super) fn clear_unified_exec_wait_tracking(&mut self) {
        self.unified_exec_wait_tracker = None;
        self.clear_unified_exec_wait_invocation_countdown();
        if is_unified_exec_process_wait_header(&self.status_state.current_status.header) {
            self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
            self.set_status_header(String::from("Working"));
        }
    }

    fn on_terminal_wait(
        &mut self,
        notification: TerminalInteractionNotification,
        wait: TerminalWait,
    ) {
        let TerminalInteractionNotification {
            thread_id: _,
            turn_id,
            item_id,
            process_id,
            stdin,
            deadline_at_ms,
            ..
        } = notification;
        match wait {
            TerminalWait::Started {
                interaction_id,
                started_at_ms,
                mode,
            } => {
                if !self.bottom_pane.is_task_running()
                    || self.turn_lifecycle.last_turn_id.as_deref() != Some(turn_id.as_str())
                {
                    return;
                }
                let wait_key = UnifiedExecWaitKey {
                    turn_id,
                    interaction_id,
                };
                if self
                    .unified_exec_wait_tracker
                    .as_ref()
                    .is_some_and(|tracker| tracker.active.contains_key(&wait_key))
                {
                    // Duplicate starts are idempotent and must not reset the original clock.
                    return;
                }
                self.flush_unified_exec_wait_streak();
                let command_display =
                    self.unified_exec_process_command_display(&item_id, &process_id);
                self.unified_exec_wait_tracker
                    .get_or_insert_with(UnifiedExecWaitTracker::default)
                    .active
                    .entry(wait_key)
                    .or_insert(ActiveTerminalWait {
                        call_id: item_id,
                        process_id,
                        command_display,
                        started_at_ms,
                        deadline_at_ms,
                        mode,
                    });
                self.refresh_unified_exec_wait_status();
                self.request_redraw();
            }
            TerminalWait::Finished {
                interaction_id,
                elapsed_ms,
                reason,
            } => {
                let wait_key = UnifiedExecWaitKey {
                    turn_id,
                    interaction_id,
                };
                if !self.bottom_pane.is_task_running()
                    || self.turn_lifecycle.last_turn_id.as_deref()
                        != Some(wait_key.turn_id.as_str())
                {
                    return;
                }
                let active_wait = self.unified_exec_wait_tracker.as_mut().and_then(|tracker| {
                    if tracker.active.get(&wait_key).is_some_and(|active| {
                        active.call_id == item_id && active.process_id == process_id
                    }) {
                        tracker.active.remove(&wait_key)
                    } else {
                        None
                    }
                });
                let Some(active_wait) = active_wait else {
                    // Ignore mismatched, duplicate, and late completions before they can alter
                    // the transcript or interrupt a newer answer stream.
                    return;
                };
                self.flush_unified_exec_wait_streak();
                let command_display = active_wait
                    .command_display
                    .or_else(|| self.unified_exec_process_command_display(&item_id, &process_id));
                if !stdin.is_empty() {
                    self.on_async_agent_notice(history_cell::new_unified_exec_interaction(
                        command_display.clone(),
                        stdin,
                    ));
                }
                self.on_async_agent_notice(history_cell::new_unified_exec_wait_result(
                    command_display,
                    process_id,
                    wait_key.interaction_id,
                    elapsed_ms,
                    reason,
                ));
                self.discard_empty_unified_exec_wait_tracker();
                self.refresh_unified_exec_wait_status();
                self.request_redraw();
            }
        }
    }

    fn unified_exec_process_command_display(
        &self,
        call_id: &str,
        process_id: &str,
    ) -> Option<String> {
        self.unified_exec_processes
            .iter()
            .find(|process| process.key == process_id || process.call_id == call_id)
            .map(|process| process.command_display.clone())
            .or_else(|| {
                self.completed_unified_exec_processes
                    .iter()
                    .rev()
                    .find(|process| process.key == process_id)
                    .map(|process| process.command_display.clone())
            })
            .filter(|command| !command.is_empty())
    }

    fn sync_unified_exec_wait_countdown(&mut self) {
        let single_timed_wait = self
            .unified_exec_wait_tracker
            .as_ref()
            .and_then(|tracker| match tracker.active.len() {
                1 => tracker.active.iter().next(),
                _ => None,
            })
            .and_then(|(key, wait)| {
                (wait.mode == TerminalWaitMode::Timed).then_some((
                    key.turn_id.clone(),
                    key.interaction_id.clone(),
                    wait.deadline_at_ms,
                ))
            });
        if let Some((turn_id, interaction_id, Some(deadline_at_ms))) = single_timed_wait {
            let owner = StatusCountdownOwner::UnifiedExecWaitInvocation {
                turn_id,
                interaction_id,
            };
            if self.status_state.countdown_owner.as_ref() != Some(&owner) {
                self.set_status_countdown_deadline_at_ms(owner, deadline_at_ms);
            }
            self.bottom_pane.set_global_status_timer_visible(true);
            return;
        }
        match self.status_state.countdown_owner.as_ref() {
            Some(StatusCountdownOwner::UnifiedExec { .. })
            | Some(StatusCountdownOwner::UnifiedExecWaitInvocation { .. }) => {
                self.clear_status_countdown();
            }
            Some(StatusCountdownOwner::CollabWait { .. })
            | Some(StatusCountdownOwner::Sleep { .. })
            | None => {}
        }
        self.bottom_pane.set_global_status_timer_visible(false);
    }

    fn clear_unified_exec_wait_invocation_countdown(&mut self) {
        if matches!(
            self.status_state.countdown_owner.as_ref(),
            Some(StatusCountdownOwner::UnifiedExecWaitInvocation { .. })
        ) {
            self.clear_status_countdown();
        }
    }

    fn discard_empty_unified_exec_wait_tracker(&mut self) {
        if self
            .unified_exec_wait_tracker
            .as_ref()
            .is_some_and(UnifiedExecWaitTracker::is_empty)
        {
            self.unified_exec_wait_tracker = None;
        }
    }
}

pub(super) fn is_unified_exec_process_wait_header(header: &str) -> bool {
    header == "Waiting for process exit"
        || header == "Waiting for timed process result"
        || header.starts_with("Waiting on ")
}
