//! One-shot guarded Legacy mutation. Ambiguous results must never be retried.

use super::*;
use crate::app_backtrack::LegacyRollbackTarget;
use codex_app_server_protocol::ThreadRollbackParams;
use codex_app_server_protocol::ThreadRollbackResponse;
use codex_app_server_protocol::thread_rollback_error_requires_refresh;
use codex_app_server_protocol::thread_rollback_error_was_committed;

#[derive(Debug)]
pub(crate) enum LegacyRollbackOutcome {
    Refreshed(Box<ThreadRollbackResponse>),
    CommittedRefreshRequired,
    Unknown,
    Rejected(TypedRequestError),
}

fn rollback_outcome(
    result: std::result::Result<ThreadRollbackResponse, TypedRequestError>,
) -> LegacyRollbackOutcome {
    match result {
        Ok(response) => LegacyRollbackOutcome::Refreshed(Box::new(response)),
        Err(TypedRequestError::Server { source, .. })
            if thread_rollback_error_was_committed(&source) =>
        {
            LegacyRollbackOutcome::CommittedRefreshRequired
        }
        Err(TypedRequestError::Server { source, .. })
            if thread_rollback_error_requires_refresh(&source) =>
        {
            LegacyRollbackOutcome::Unknown
        }
        Err(
            err @ TypedRequestError::Server {
                source:
                    codex_app_server_protocol::JSONRPCErrorError {
                        code: -32602..=-32600,
                        ..
                    },
                ..
            },
        ) => LegacyRollbackOutcome::Rejected(err),
        Err(TypedRequestError::Server { .. }) => LegacyRollbackOutcome::Unknown,
        Err(TypedRequestError::Transport { .. } | TypedRequestError::Deserialize { .. }) => {
            LegacyRollbackOutcome::Unknown
        }
    }
}

impl AppServerSession {
    pub(crate) async fn rollback_legacy_thread(
        &mut self,
        thread_id: ThreadId,
        target: LegacyRollbackTarget,
    ) -> (RequestId, LegacyRollbackOutcome) {
        let request_id = self.next_request_id();
        let outcome = rollback_outcome(
            self.client
                .request_typed(ClientRequest::ThreadRollback {
                    request_id: request_id.clone(),
                    params: ThreadRollbackParams {
                        thread_id: thread_id.to_string(),
                        num_turns: target.num_turns,
                        expected_start_turn_id: Some(target.expected_start_turn_id),
                        expected_turn_count: Some(target.expected_turn_count),
                    },
                })
                .await,
        );
        if matches!(
            &outcome,
            LegacyRollbackOutcome::Refreshed(_) | LegacyRollbackOutcome::CommittedRefreshRequired
        ) {
            self.history_pagination.remove(&thread_id);
        }
        (request_id, outcome)
    }
}

#[cfg(test)]
#[path = "rollback_tests.rs"]
mod tests;
