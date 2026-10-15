//! Continuation is a core admission operation, never a synthetic user message.

use super::*;
use codex_app_server_protocol::TurnContinueParams;
use codex_app_server_protocol::TurnContinueResponse;
use codex_core::NotSubmittedReason;
use codex_core::StartIfIdleSubmission;

impl TurnRequestProcessor {
    pub(crate) async fn turn_continue(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnContinueParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let submission = thread
            .continue_turn()
            .await
            .map_err(|error| invalid_request(format!("cannot continue turn: {error}")))?;
        let turn_id = match submission {
            StartIfIdleSubmission::Started { turn_id, .. } => turn_id,
            StartIfIdleSubmission::NotSubmitted {
                reason: NotSubmittedReason::ServerDraining,
            } => return Err(crate::error_code::server_draining_error()),
            StartIfIdleSubmission::NotSubmitted { reason } => {
                return Err(invalid_request(format!("cannot continue turn: {reason:?}")));
            }
        };
        self.outgoing
            .record_request_turn_id(request_id, &turn_id)
            .await;
        Ok(Some(TurnContinueResponse { turn_id }.into()))
    }
}
