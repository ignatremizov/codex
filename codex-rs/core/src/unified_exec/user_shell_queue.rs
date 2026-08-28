//! Submission ordering and pending-wake tracking for user-authored shell commands.

use codex_protocol::protocol::UserShellCommandFinalDelivery;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecProcessManager;
use super::UserShellSubmission;
use super::UserShellSubmissionPhase;

impl UnifiedExecProcessManager {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "shell wake publication must remain atomic with automatic idle-turn reservation"
    )]
    pub(crate) async fn reserve_user_shell_submission(
        &self,
        final_delivery: UserShellCommandFinalDelivery,
    ) -> u64 {
        let _wake_reservation_permit = self.acquire_user_shell_wake_reservation_permit().await;
        let mut store = self.process_store.lock().await;
        let submission_id = store.next_user_shell_submission_id;
        store.next_user_shell_submission_id += 1;
        store.pending_user_shell_submissions.insert(
            submission_id,
            UserShellSubmission {
                final_delivery,
                phase: UserShellSubmissionPhase::Pending,
            },
        );
        submission_id
    }

    /// Waits for this submission's ordering boundary and atomically claims its launch.
    ///
    /// Targeted cancellation removes a pending submission under the same process-store lock. The
    /// stop request or this launch claim therefore wins deterministically; once the claim wins,
    /// later cancellation applies to a launching or running command.
    pub(crate) async fn wait_for_user_shell_launch(
        &self,
        submission_id: u64,
        queue_command: bool,
        cancellation_token: &CancellationToken,
    ) -> bool {
        loop {
            let changed = self.user_shell_submission_changed.notified();
            {
                let mut store = self.process_store.lock().await;
                if cancellation_token.is_cancelled() {
                    return false;
                }
                let has_earlier_submission = queue_command
                    && store
                        .pending_user_shell_submissions
                        .range(..submission_id)
                        .next()
                        .is_some();
                if !has_earlier_submission {
                    let Some(submission) =
                        store.pending_user_shell_submissions.get_mut(&submission_id)
                    else {
                        return false;
                    };
                    submission.phase = UserShellSubmissionPhase::Launching;
                    return true;
                }
            }
            tokio::select! {
                _ = cancellation_token.cancelled() => return false,
                () = changed => {}
            }
        }
    }

    pub(crate) async fn release_user_shell_submission(&self, submission_id: u64) {
        let removed = self
            .process_store
            .lock()
            .await
            .pending_user_shell_submissions
            .remove(&submission_id)
            .is_some();
        if removed {
            self.user_shell_submission_changed.notify_waiters();
        }
    }

    pub(crate) async fn has_pending_user_shell_completion_wake(&self) -> bool {
        self.process_store
            .lock()
            .await
            .pending_user_shell_submissions
            .values()
            .any(|submission| submission.final_delivery == UserShellCommandFinalDelivery::Wake)
    }
}

#[cfg(test)]
#[path = "user_shell_queue_tests.rs"]
mod tests;
