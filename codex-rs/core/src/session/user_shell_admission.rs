//! User-shell result publication belongs to the exact session that accepts its command.

use super::SubmissionAdmission;
use super::command_approval::QueuedSubmission;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use std::sync::Arc;

impl QueuedSubmission {
    /// Called under submission ordering, before channel acceptance can race a queued shutdown.
    /// Cancellation or a rejected send drops this capability with the unaccepted submission.
    pub(super) fn accept_user_shell_completion(
        &mut self,
        admission: &Arc<SubmissionAdmission>,
    ) -> CodexResult<()> {
        self.user_shell_completion =
            if matches!(&self.submission.op, Op::RunUserShellCommand { .. }) {
                Some(admission.try_accept_completion_delivery().ok_or_else(|| {
                    CodexErr::InvalidRequest("user-shell completion admission is closed".into())
                })?)
            } else {
                None
            };
        Ok(())
    }
}

#[cfg(test)]
#[path = "user_shell_admission_tests.rs"]
mod tests;
