//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::TurnStartOptions;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::types::MessageDeliveryMode;
use crate::config::MultiAgentMessageDelivery;
use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::multi_agents_spec::MAX_AGENT_MESSAGE_PAYLOAD_BYTES;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    #[serde(default, deserialize_with = "deserialize_task_message")]
    pub(crate) task_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    #[serde(default, deserialize_with = "deserialize_task_message")]
    pub(crate) task_message: Option<String>,
}

/// Omission is optional, but a supplied audit field must be a string, not null.
pub(super) fn deserialize_task_message<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

pub(super) fn prepare_agent_message(
    message: String,
    task_message: Option<String>,
    message_delivery: MultiAgentMessageDelivery,
    source: &crate::tools::context::ToolCallSource,
) -> Result<AgentMessage, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    // Trusted direct plaintext is a stored representation, not model-produced ciphertext.
    let message_delivery = if matches!(
        source,
        crate::tools::context::ToolCallSource::DirectPlaintextMessage
    ) {
        MultiAgentMessageDelivery::Plaintext
    } else {
        message_delivery
    };
    match message_delivery {
        MultiAgentMessageDelivery::Encrypted => {
            if task_message.is_some() {
                return Err(FunctionCallError::RespondToModel(
                    "task_message is only supported when message_delivery is encrypted_with_audit"
                        .to_string(),
                ));
            }
            validate_message_payload_size(&message, /*task_message*/ None)?;
            Ok(AgentMessage::Encrypted(message))
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
            Ok(AgentMessage::EncryptedWithAudit {
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
            Ok(AgentMessage::Plaintext(message))
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
pub(super) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    task_message: Option<String>,
    message_delivery: MultiAgentMessageDelivery,
    analytics: &mut ToolCallAnalytics,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let prepared_message =
        prepare_agent_message(message, task_message, message_delivery, &invocation.source)?;
    let ToolInvocation {
        session,
        turn,
        call_id,
        ..
    } = invocation;
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    analytics.set_receiver(receiver_thread_id);
    let resume_config =
        build_agent_resume_config(&turn).map_err(FunctionCallError::RespondToModel)?;
    let receipt = session
        .services
        .agent_control
        .send(SendRequest {
            caller: session.thread_id,
            target: AgentTarget::Id(receiver_thread_id),
            resume_config,
            input: AgentInput::Message {
                message: prepared_message,
                mode,
            },
            start_options: TurnStartOptions {
                parent_turn_id: (mode == MessageDeliveryMode::TriggerTurn)
                    .then(|| turn.sub_id.clone()),
                root_turn_id: turn.turn_metadata_state.root_turn_id(),
                turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                cyber_access_program: turn.cyber_access_program,
                ..Default::default()
            },
        })
        .await
        .map_err(|err| collab_v2_agent_error(receiver_thread_id, err))?;
    let receiver_agent_path = receipt.metadata.agent_path.ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    })?;
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
