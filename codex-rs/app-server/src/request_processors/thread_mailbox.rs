//! User mailbox transport; Core owns durable acceptance and receiver eligibility.

use super::thread_processor::ThreadRequestProcessor;
use super::turn_processor::validate_user_input_image_urls;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::error_code::method_not_found;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadMailboxAddParams;
use codex_app_server_protocol::ThreadMailboxAddResponse;
use codex_app_server_protocol::ThreadMailboxMessageState;
use codex_app_server_protocol::ThreadMailboxPendingSender;
use codex_app_server_protocol::ThreadMailboxReadParams;
use codex_app_server_protocol::ThreadMailboxReadResponse;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::MailboxMessageState;
use codex_thread_store::MailboxSender;
use codex_thread_store::ThreadStoreError;

impl ThreadRequestProcessor {
    pub(crate) async fn thread_mailbox_add(
        &self,
        params: ThreadMailboxAddParams,
    ) -> Result<ThreadMailboxAddResponse, JSONRPCErrorError> {
        validate_user_input_image_urls(&params.input)?;
        let receiver = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        let accepted = self
            .thread_manager
            .accept_user_mailbox_input(
                receiver,
                params.input.into_iter().map(UserInput::into_core).collect(),
                params.client_user_message_id,
            )
            .await
            .map_err(|error| match error.details() {
                CodexErrorDetails::InvalidRequest(message)
                | CodexErrorDetails::UnsupportedOperation(message) => {
                    invalid_request(message.clone())
                }
                CodexErrorDetails::ThreadNotFound(thread_id) => {
                    invalid_request(format!("thread not found: {thread_id}"))
                }
                _ => internal_error(format!("mailbox acceptance failed: {error}")),
            })?;
        Ok(ThreadMailboxAddResponse {
            message_id: accepted.id,
            state: match accepted.state {
                MailboxMessageState::Pending => ThreadMailboxMessageState::Pending,
                MailboxMessageState::Claimed => ThreadMailboxMessageState::Claimed,
                MailboxMessageState::Consumed => ThreadMailboxMessageState::Consumed,
                MailboxMessageState::Rejected => ThreadMailboxMessageState::Rejected,
            },
            rejection_reason: accepted.rejection_reason,
        })
    }

    pub(crate) async fn thread_mailbox_read(
        &self,
        params: ThreadMailboxReadParams,
    ) -> Result<ThreadMailboxReadResponse, JSONRPCErrorError> {
        let receiver = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        let inventory = self
            .thread_store
            .read_mailbox_inventory(receiver)
            .await
            .map_err(|error| match error {
                ThreadStoreError::Unsupported { .. } => {
                    method_not_found("thread/mailbox/read is unavailable without mailbox storage")
                }
                ThreadStoreError::InvalidRequest { message } => invalid_request(message),
                error => internal_error(format!("failed to read mailbox inventory: {error}")),
            })?;
        let pending_total = inventory
            .pending_senders
            .iter()
            .try_fold(0_u64, |total, sender| total.checked_add(sender.count))
            .ok_or_else(|| internal_error("pending mailbox total exceeds u64"))?;
        let pending_senders = inventory
            .pending_senders
            .into_iter()
            .map(|sender| match sender.sender {
                MailboxSender::User => ThreadMailboxPendingSender::User {
                    count: sender.count,
                },
                MailboxSender::Agent(thread_id) => ThreadMailboxPendingSender::Agent {
                    thread_id: thread_id.to_string(),
                    count: sender.count,
                },
            })
            .collect();
        Ok(ThreadMailboxReadResponse {
            pending_total,
            pending_senders,
        })
    }
}
