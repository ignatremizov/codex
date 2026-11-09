//! Schedule admission and receipt lookup reuse ordinary queue and history owners.

use super::*;
use codex_app_server_protocol::ThreadQueueResultParams;
use codex_app_server_protocol::ThreadQueueResultResponse;
use codex_app_server_protocol::ThreadQueueScheduleParams;
use codex_thread_store::QueuedSubmissionSchedule;

impl ThreadQueueRequestProcessor {
    pub(crate) async fn schedule(
        &self,
        params: ThreadQueueScheduleParams,
    ) -> Result<ThreadQueueAddResponse, JSONRPCErrorError> {
        if params.input.is_empty()
            || params
                .input
                .iter()
                .any(|item| !matches!(item, UserInput::Text { .. }))
        {
            return Err(invalid_request("scheduled input must contain text only"));
        }
        let (thread_id, loaded, source) = self.require_thread(&params.thread_id).await?;
        ensure_direct_input_allowed(loaded.as_deref(), &source)?;
        let config = self.thread_processor.config.as_ref();
        let profile = config
            .auth_file_selection
            .profile_identity(&config.codex_home, config.cli_auth_credentials_store_mode);
        if config.cli_auth_credentials_store_mode
            == codex_login::AuthCredentialsStoreMode::Ephemeral
        {
            return Err(invalid_request(
                "scheduled input requires persistent credentials",
            ));
        }
        let queued_item = self
            .service()?
            .schedule(
                thread_id,
                submission_into_turn_input(params.input, Some(params.client_user_message_id)),
                QueuedSubmissionSchedule {
                    scheduled_at: params.scheduled_at,
                    fallback_profile: profile.profile_opaque_id,
                },
            )
            .await
            .map_err(queue_error)?;
        Ok(ThreadQueueAddResponse {
            queued_submission: api_queued_submission(queued_item)?,
        })
    }

    pub(crate) async fn result(
        &self,
        params: ThreadQueueResultParams,
    ) -> Result<ThreadQueueResultResponse, JSONRPCErrorError> {
        let service = self.service()?;
        if let Some((thread_id, pending)) = service
            .pending_submission(params.queued_submission_id.clone())
            .await
            .map_err(queue_error)?
        {
            return Ok(ThreadQueueResultResponse {
                queued_submission: Some(api_queued_submission(pending)?),
                thread_id: Some(thread_id.to_string()),
                turn_id: None,
                started: None,
                turn: None,
            });
        }
        let receipt = service
            .receipt(params.queued_submission_id)
            .await
            .map_err(queue_error)?
            .ok_or_else(|| {
                invalid_request("scheduled input not found or no admission was confirmed")
            })?;
        let turn = self
            .thread_processor
            .queued_input_turn(receipt.thread_id, &receipt.turn_id)
            .await?;
        Ok(ThreadQueueResultResponse {
            queued_submission: None,
            thread_id: Some(receipt.thread_id.to_string()),
            turn_id: Some(receipt.turn_id),
            started: Some(receipt.started),
            turn,
        })
    }
}
