//! Advisory wait estimates scoped to the lifecycle that owns the visible status.
//!
//! The server may extend its actual wait while paused. This estimate is not a scheduler:
//! expiry restores the existing elapsed display. A dropped individual poll without a subsequent
//! notification can retain its estimate until expiry; turn finalization always clears it.

use super::*;

impl ChatWidget {
    pub(super) fn set_status_countdown_deadline_at_ms(
        &mut self,
        owner: StatusCountdownOwner,
        deadline_at_ms: i64,
    ) {
        let turn_id = match &owner {
            StatusCountdownOwner::CollabWait { turn_id, .. }
            | StatusCountdownOwner::UnifiedExec { turn_id, .. } => turn_id,
        };
        if self
            .turn_lifecycle
            .last_turn_id
            .as_ref()
            .is_some_and(|current| current != turn_id)
        {
            return;
        }
        if self.status_state.compaction.is_some()
            || self.status_state.retry_status_header.is_some()
            || !self.status_state.pending_guardian_review_status.is_empty()
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
