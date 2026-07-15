//! Converts agent messages and orders acceptance against the captured recipient runtime.
//! Queue-only mail for locally evicted agents stays unloaded; reload publication and
//! replacement registrations cannot redirect an already captured submission.

use super::LocalAgentControl;
use super::non_empty_task_message;
use crate::TurnStartOptions;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::MessageDelivery;
use crate::codex_thread::CodexThread;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use std::sync::Arc;

impl AgentMessage {
    pub(crate) fn into_communication(
        self,
        author: AgentPath,
        recipient: AgentPath,
        mode: MessageDeliveryMode,
    ) -> InterAgentCommunication {
        let trigger_turn = mode == MessageDeliveryMode::TriggerTurn;
        match self {
            Self::Encrypted(message) => InterAgentCommunication::new_encrypted(
                author,
                recipient,
                Vec::new(),
                message,
                trigger_turn,
            ),
            Self::EncryptedWithAudit {
                encrypted_content,
                audit_content,
            } => {
                let mut communication = InterAgentCommunication::new_encrypted(
                    author,
                    recipient,
                    Vec::new(),
                    encrypted_content,
                    trigger_turn,
                );
                communication.content = audit_content;
                communication
            }
            Self::Plaintext(message) => {
                let message_type = match mode {
                    MessageDeliveryMode::QueueOnly => InterAgentMessageType::Message,
                    MessageDeliveryMode::TriggerTurn => InterAgentMessageType::NewTask,
                };
                let content = InterAgentMessage::new(
                    message_type,
                    recipient.clone(),
                    author.clone(),
                    message,
                )
                .render()
                .into_text()
                .unwrap_or_default();
                InterAgentCommunication::new(author, recipient, Vec::new(), content, trigger_turn)
            }
        }
    }
}

/// Distinguishes a captured live runtime from a queue-only unloaded recipient.
pub(super) enum CapturedRecipient {
    Loaded(Arc<CodexThread>),
    Unloaded,
}

impl LocalAgentControl {
    pub(crate) async fn send_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let state = self.runtime.upgrade()?;
        let recipient = match state.threads.read().await.get(&agent_id).cloned() {
            Some(thread) => CapturedRecipient::Loaded(thread),
            None if !communication.trigger_turn => CapturedRecipient::Unloaded,
            None => return Err(CodexErr::ThreadNotFound(agent_id)),
        };
        if communication.trigger_turn
            && let CapturedRecipient::Loaded(thread) = &recipient
        {
            thread
                .ensure_execution_capacity_for_turn_start(self)
                .await?;
        }
        self.send_inter_agent_communication_after_capacity_check(
            agent_id,
            &state,
            recipient,
            communication,
            agent_communication_context,
            start_options,
        )
        .await
    }

    pub(super) async fn send_inter_agent_communication_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        recipient: CapturedRecipient,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        self.submit_inter_agent_communication(
            agent_id,
            state,
            recipient,
            communication,
            context,
            start_options,
        )
        .await
    }

    async fn submit_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        recipient: CapturedRecipient,
        mut communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let submission = self.runtime.registry.mailbox_submission(agent_id);
        let _submission_permit = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}"))
            })?;
        let observers = state.extensions.message_lifecycle_contributors();
        let delivery = (!observers.is_empty()).then(|| {
            let message_id = communication
                .id
                .take()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| ResponseItemId::new("amsg"));
            communication.id = Some(message_id.clone());
            MessageDelivery::publish(context.sender_thread_id, message_id, observers)
        });
        let last_task_message = context
            .updates_last_task_message()
            .then(|| non_empty_task_message(communication.content.clone()));
        let communication_for_log =
            crate::agent_communication::logging_enabled().then(|| communication.clone());
        let thread = match recipient {
            CapturedRecipient::Loaded(thread) => thread,
            CapturedRecipient::Unloaded => {
                // Serialize queue-only acceptance with publication of a reloaded runtime.
                let threads = state.threads.read().await;
                if threads.contains_key(&agent_id)
                    || !self
                        .runtime
                        .registry
                        .submission_is_current(agent_id, &submission)
                {
                    return Err(CodexErr::ThreadNotFound(agent_id));
                }
                let _membership = self.runtime.admit_start()?;
                self.runtime.ensure_agent_known(agent_id)?;
                let submission_id = uuid::Uuid::now_v7().to_string();
                self.runtime.mailboxes.enqueue(
                    agent_id,
                    Some(submission_id.clone()),
                    vec![crate::PendingAgentMessage::new(communication, delivery)],
                )?;
                if let Some(last_task_message) = last_task_message {
                    self.runtime.registry.update_last_task_message(
                        agent_id,
                        &submission,
                        last_task_message,
                    );
                }
                if let Some(communication) = communication_for_log {
                    crate::agent_communication::emit_agent_communication_send(
                        &submission_id,
                        &context,
                        &communication,
                        agent_id,
                    );
                }
                return Ok(submission_id);
            }
        };
        let current_thread = state.get_thread(agent_id).await?;
        if !Arc::ptr_eq(&thread, &current_thread)
            || !self
                .runtime
                .registry
                .submission_is_current(agent_id, &submission)
        {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        // Loaded recipients retain submission ordering with follow-ups and interrupts.
        let (parent_turn_id, root_turn_id) = if communication.trigger_turn {
            (
                start_options.parent_turn_id.clone(),
                start_options.root_turn_id.clone(),
            )
        } else {
            (None, None)
        };
        let result = self
            .handle_thread_request_result(
                agent_id,
                state,
                &thread,
                state
                    .send_op_to_thread(
                        &thread,
                        Op::InterAgentCommunication {
                            communication,
                            start_options,
                        },
                        parent_turn_id,
                        root_turn_id,
                        delivery,
                    )
                    .await,
            )
            .await;
        if let (Some(communication), Ok(communication_id)) =
            (communication_for_log, result.as_ref())
        {
            crate::agent_communication::emit_agent_communication_send(
                communication_id,
                &context,
                &communication,
                agent_id,
            );
        }
        if result.is_ok()
            && let Some(last_task_message) = last_task_message
        {
            self.runtime.registry.update_last_task_message(
                agent_id,
                &submission,
                last_task_message,
            );
        }
        result
    }
}
