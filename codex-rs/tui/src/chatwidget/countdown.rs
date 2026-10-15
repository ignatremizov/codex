//! Advisory wait estimates scoped to the lifecycle that owns the visible status.
//!
//! The server may extend its actual wait while paused. This estimate is not a scheduler:
//! expiry remains at zero until the owning lifecycle clears it. A dropped individual poll without
//! a subsequent notification can retain that estimate; turn finalization always clears it.

use super::*;

impl ChatWidget {
    pub(super) fn restore_retry_status_header_if_present(
        &mut self,
        notification: &ServerNotification,
    ) {
        if let Some(StatusCountdownOwner::ModelRetry { turn_id }) =
            &self.status_state.countdown_owner
        {
            // Background notifications (account limits, names, completed shell jobs) do not
            // establish that this model request recovered. Keep its countdown until progress
            // or a lifecycle transition for the owning turn.
            let (event_thread, event_turn) = match notification {
                ServerNotification::AgentMessageDelta(event) => (&event.thread_id, &event.turn_id),
                ServerNotification::PlanDelta(event) => (&event.thread_id, &event.turn_id),
                ServerNotification::ReasoningSummaryTextDelta(event) => {
                    (&event.thread_id, &event.turn_id)
                }
                ServerNotification::ReasoningTextDelta(event) => (&event.thread_id, &event.turn_id),
                ServerNotification::ReasoningSummaryPartAdded(event) => {
                    (&event.thread_id, &event.turn_id)
                }
                ServerNotification::ModelVerification(event) => (&event.thread_id, &event.turn_id),
                ServerNotification::ContextCompactionStatus(event) => {
                    (&event.thread_id, &event.turn_id)
                }
                ServerNotification::ItemStarted(event) => (&event.thread_id, &event.turn_id),
                ServerNotification::ItemCompleted(event)
                    if matches!(
                        &event.item,
                        ThreadItem::AgentMessage { .. }
                            | ThreadItem::Reasoning { .. }
                            | ThreadItem::Plan { .. }
                            | ThreadItem::ContextCompaction { .. }
                    ) =>
                {
                    (&event.thread_id, &event.turn_id)
                }
                ServerNotification::TurnCompleted(event) => (&event.thread_id, &event.turn.id),
                ServerNotification::Error(event) => (&event.thread_id, &event.turn_id),
                _ => return,
            };
            if event_turn != turn_id
                || self
                    .thread_id()
                    .is_some_and(|id| id.to_string() != *event_thread)
            {
                return;
            }
        }
        if let Some(header) = self.status_state.take_retry_status_header() {
            if matches!(
                self.status_state.countdown_owner,
                Some(StatusCountdownOwner::ModelRetry { .. })
            ) {
                self.clear_status_countdown();
            }
            self.set_status_header(header);
        }
    }

    pub(super) fn set_status_countdown_deadline_at_ms(
        &mut self,
        owner: StatusCountdownOwner,
        deadline_at_ms: i64,
    ) {
        let turn_id = match &owner {
            StatusCountdownOwner::ModelRetry { turn_id }
            | StatusCountdownOwner::CollabWait { turn_id, .. }
            | StatusCountdownOwner::UnifiedExec { turn_id, .. }
            | StatusCountdownOwner::UnifiedExecWaitInvocation { turn_id, .. }
            | StatusCountdownOwner::Sleep { turn_id, .. } => turn_id,
        };
        if self
            .turn_lifecycle
            .last_turn_id
            .as_ref()
            .is_some_and(|current| current != turn_id)
        {
            return;
        }
        let is_model_retry = matches!(owner, StatusCountdownOwner::ModelRetry { .. });
        if (!is_model_retry
            && (self.status_state.compaction.is_some()
                || self.status_state.retry_status_header.is_some()
                || !self.status_state.pending_guardian_review_status.is_empty()))
            || !self.bottom_pane.is_task_running()
            || !self.turn_lifecycle.agent_turn_running
        {
            self.clear_status_countdown();
            return;
        }
        let deadline =
            deadline_at_ms_to_instant(deadline_at_ms, std::time::SystemTime::now(), Instant::now());
        self.status_state.countdown_owner = deadline.map(|_| owner);
        self.bottom_pane.update_status_countdown_deadline(deadline);
    }

    pub(super) fn clear_status_countdown_if_owner(&mut self, owner: &StatusCountdownOwner) -> bool {
        if self.status_state.countdown_owner.as_ref() != Some(owner) {
            return false;
        }
        self.clear_status_countdown();
        true
    }

    pub(super) fn clear_status_countdown(&mut self) {
        self.status_state.countdown_owner = None;
        self.bottom_pane
            .update_status_countdown_deadline(/*deadline*/ None);
    }
}

fn deadline_at_ms_to_instant(
    deadline_at_ms: i64,
    wall_now: std::time::SystemTime,
    now: Instant,
) -> Option<Instant> {
    let now_ms = i64::try_from(
        wall_now
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    let remaining_ms = deadline_at_ms.checked_sub(now_ms)?.max(0);
    now.checked_add(Duration::from_millis(u64::try_from(remaining_ms).ok()?))
}

#[cfg(test)]
#[path = "countdown_tests.rs"]
mod tests;
