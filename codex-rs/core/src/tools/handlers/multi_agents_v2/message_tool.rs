//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::*;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::MultiAgentMessageDelivery;
use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::multi_agents_spec::MAX_AGENT_MESSAGE_PAYLOAD_BYTES;
use codex_protocol::protocol::InterAgentCommunication;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageDeliveryMode {
    QueueOnly,
    TriggerTurn,
}

impl MessageDeliveryMode {
    /// Returns whether the produced communication should start a turn immediately.
    fn apply(self, communication: InterAgentCommunication) -> InterAgentCommunication {
        match self {
            Self::QueueOnly => InterAgentCommunication {
                trigger_turn: false,
                ..communication
            },
            Self::TriggerTurn => InterAgentCommunication {
                trigger_turn: true,
                ..communication
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    pub(crate) task_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    pub(crate) task_message: Option<String>,
}

#[derive(Debug)]
pub(super) enum PreparedAgentMessage {
    Encrypted {
        encrypted_content: String,
    },
    EncryptedWithAudit {
        encrypted_content: String,
        audit_content: String,
    },
    Plaintext {
        content: String,
    },
}

impl PreparedAgentMessage {
    pub(super) fn from_tool_args(
        message: String,
        task_message: Option<String>,
        message_delivery: MultiAgentMessageDelivery,
    ) -> Result<Self, FunctionCallError> {
        if message.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "Empty message can't be sent to an agent".to_string(),
            ));
        }
        match message_delivery {
            MultiAgentMessageDelivery::Encrypted => {
                if task_message.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "task_message is only supported when message_delivery is encrypted_with_audit"
                            .to_string(),
                    ));
                }
                validate_message_payload_size(&message, /*task_message*/ None)?;
                Ok(Self::Encrypted {
                    encrypted_content: message,
                })
            }
            MultiAgentMessageDelivery::EncryptedWithAudit => {
                let Some(task_message) = task_message else {
                    return Err(FunctionCallError::RespondToModel(
                        "task_message is required when message_delivery is encrypted_with_audit"
                            .to_string(),
                    ));
                };
                if task_message.trim().is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "task_message must not be empty".to_string(),
                    ));
                }
                validate_message_payload_size(&message, Some(&task_message))?;
                Ok(Self::EncryptedWithAudit {
                    encrypted_content: message,
                    audit_content: task_message,
                })
            }
            MultiAgentMessageDelivery::Plaintext => {
                if task_message.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "task_message is only supported when message_delivery is encrypted_with_audit"
                            .to_string(),
                    ));
                }
                validate_message_payload_size(&message, /*task_message*/ None)?;
                Ok(Self::Plaintext { content: message })
            }
        }
    }

    pub(super) fn into_communication(
        self,
        author: AgentPath,
        recipient: AgentPath,
    ) -> InterAgentCommunication {
        match self {
            Self::Encrypted { encrypted_content } => InterAgentCommunication::new_encrypted(
                author,
                recipient,
                Vec::new(),
                encrypted_content,
                /*trigger_turn*/ true,
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
                    /*trigger_turn*/ true,
                );
                communication.content = audit_content;
                communication
            }
            Self::Plaintext { content } => InterAgentCommunication::new(
                author,
                recipient,
                Vec::new(),
                content,
                /*trigger_turn*/ true,
            ),
        }
    }
}

fn validate_message_payload_size(
    message: &str,
    task_message: Option<&str>,
) -> Result<(), FunctionCallError> {
    let payload_bytes = message
        .len()
        .saturating_add(task_message.map_or(0, str::len));
    if payload_bytes > MAX_AGENT_MESSAGE_PAYLOAD_BYTES {
        let fields = if task_message.is_some() {
            "combined message and task_message"
        } else {
            "message"
        };
        return Err(FunctionCallError::RespondToModel(format!(
            "{fields} payload must not exceed {MAX_AGENT_MESSAGE_PAYLOAD_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Handles the shared MultiAgentV2 message flow for both `send_message` and `followup_task`.
pub(crate) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    task_message: Option<String>,
    message_delivery: MultiAgentMessageDelivery,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let prepared_message =
        PreparedAgentMessage::from_tool_args(message, task_message, message_delivery)?;
    let ToolInvocation {
        session,
        turn,
        call_id,
        ..
    } = invocation;
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    let receiver_agent = session
        .services
        .agent_control
        .ensure_agent_known(receiver_thread_id)
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    if mode == MessageDeliveryMode::TriggerTurn
        && receiver_agent
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root)
    {
        return Err(FunctionCallError::RespondToModel(
            "Follow-up tasks can't target the root agent".to_string(),
        ));
    }
    let receiver_agent_path = receiver_agent.agent_path.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    })?;
    let resume_config = build_agent_resume_config(turn.as_ref())?;
    session
        .services
        .agent_control
        .ensure_v2_agent_loaded(resume_config, receiver_thread_id)
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let communication = prepared_message.into_communication(author, receiver_agent_path.clone());
    let kind = match mode {
        MessageDeliveryMode::QueueOnly => AgentCommunicationKind::Message,
        MessageDeliveryMode::TriggerTurn => AgentCommunicationKind::Followup,
    };
    let context = AgentCommunicationContext::new(kind, session.thread_id);
    let result = session
        .services
        .agent_control
        .send_inter_agent_communication(receiver_thread_id, mode.apply(communication), context)
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err));
    result?;
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            id: call_id,
            agent_thread_id: receiver_thread_id,
            agent_path: receiver_agent_path,
            kind: SubAgentActivityKind::Interacted,
        },
    )
    .await;

    Ok(FunctionToolOutput::from_text(String::new(), Some(true)))
}

#[cfg(test)]
#[path = "message_tool_tests.rs"]
mod tests;
