use super::*;
use crate::session::session::Session;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::session_prefix::format_subagent_notification_message;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::sub_agent_completion_item_with_visibility;
use std::collections::HashSet;

/// Source-relative state needed to replay a completed response after close commits.
pub(crate) struct CloseAgentResponseContext {
    source_session: Arc<Session>,
    source_thread: Option<Arc<CodexThread>>,
    source: AgentPath,
    target: AgentPath,
    agent: AgentContextIdentity,
    source_multi_agent_version: MultiAgentVersion,
    target_thread_id: ThreadId,
    target_references: Vec<String>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseAgentResponseDisposition {
    NotApplicable,
    Suppressed,
    AlreadyVisible,
    Delivered,
    Queued,
    PresentationOnly,
}

/// Status captured while the target lifecycle boundary was held.
pub(crate) struct ClosedAgent {
    pub(crate) previous_status: AgentStatus,
}

impl AgentControl {
    pub(crate) async fn prepare_close_agent_response(
        &self,
        source_session: Arc<Session>,
        source_multi_agent_version: MultiAgentVersion,
        target_thread_id: ThreadId,
    ) -> CodexResult<CloseAgentResponseContext> {
        let source_thread_id = source_session.thread_id();
        let source_thread = match self.upgrade() {
            Ok(state) => state
                .get_thread_including_pending(source_thread_id)
                .await
                .ok()
                .filter(|thread| {
                    thread.session.presentation_id() == source_session.presentation_id()
                }),
            Err(_) => None,
        };
        let source = self
            .observation_agent_path(source_thread_id)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "source agent {source_thread_id} has no usable agent path"
                ))
            })?;
        let target = self
            .observation_agent_path(target_thread_id)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "target agent {target_thread_id} has no usable agent path"
                ))
            })?;
        let agent = match self
            .model_visible_agent_identity_for_version(source_multi_agent_version, target_thread_id)
            .await
        {
            Ok(agent) => agent,
            Err(err) => {
                tracing::warn!(
                    %target_thread_id,
                    "failed to resolve close response display identity; using canonical ID: {err}"
                );
                AgentContextIdentity::Canonical {
                    agent_id: target_thread_id,
                }
            }
        };
        let mut target_references = Vec::new();
        for (field, value) in agent.json_fields() {
            let Some(value) = value.as_str() else {
                continue;
            };
            target_references.push(value.to_string());
            let prefix = match field.as_str() {
                "agent_id" => Some("id"),
                "ref" => Some("ref"),
                "nickname" => Some("nick"),
                "agent_path" => None,
                _ => None,
            };
            if let Some(prefix) = prefix {
                target_references.push(format!("{prefix}:{value}"));
            }
        }
        let target_path = target.to_string();
        if !target_references.contains(&target_path) {
            target_references.push(target_path);
        }
        Ok(CloseAgentResponseContext {
            source_session,
            source_thread,
            source,
            target,
            agent,
            source_multi_agent_version,
            target_thread_id,
            target_references,
        })
    }
}

impl CloseAgentResponseContext {
    pub(crate) async fn deliver(
        self,
        status: &AgentStatus,
        policy: ResponseObservationPolicy,
    ) -> CloseAgentResponseDisposition {
        let payload = match status {
            AgentStatus::Completed(Some(message)) if !message.is_empty() => message.as_str(),
            AgentStatus::Completed(None)
            | AgentStatus::Completed(Some(_))
            | AgentStatus::Errored(_)
            | AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Interrupted
            | AgentStatus::Shutdown
            | AgentStatus::NotFound => {
                return CloseAgentResponseDisposition::NotApplicable;
            }
        };
        if policy.final_response() == FinalResponseObservation::None {
            return CloseAgentResponseDisposition::Suppressed;
        }
        let message = match self.source_multi_agent_version {
            MultiAgentVersion::V1 => {
                format_subagent_notification_message(self.agent.clone(), status)
            }
            MultiAgentVersion::Disabled | MultiAgentVersion::V2 => {
                let Some(message) = format_inter_agent_completion_message(
                    self.source.clone(),
                    self.target.clone(),
                    status,
                ) else {
                    return CloseAgentResponseDisposition::NotApplicable;
                };
                message
            }
        };
        if effective_history_contains_completion(
            &self.source_session,
            self.source.as_str(),
            &self.target_references,
            payload,
            &message,
        )
        .await
        {
            return CloseAgentResponseDisposition::AlreadyVisible;
        }
        if policy.final_response() == FinalResponseObservation::PresentationOnly {
            if let Some(source_thread) = self.source_thread.as_ref() {
                source_thread
                    .emit_sub_agent_completion_without_turn(
                        &self.target_thread_id.to_string(),
                        status,
                        SubAgentCompletionModelVisibility::NotVisible,
                    )
                    .await;
            } else if let Some(item) = sub_agent_completion_item_with_visibility(
                &self.target_thread_id.to_string(),
                status,
                SubAgentCompletionModelVisibility::NotVisible,
            ) {
                let history_only_turn_id = uuid::Uuid::now_v7().to_string();
                if let Err(err) = self
                    .source_session
                    .emit_turn_item_completed_without_turn_with_history_id(
                        TurnItem::AgentMessage(item),
                        &history_only_turn_id,
                    )
                    .await
                {
                    tracing::warn!(
                        target_thread_id = %self.target_thread_id,
                        "failed to persist close response presentation: {err}"
                    );
                }
            }
            return CloseAgentResponseDisposition::PresentationOnly;
        }

        // Close replay is a live-session convenience, not a restored durable observer. Like the
        // user-authored next-turn queue, an unconsumed replay does not survive process shutdown;
        // the closed target rollout remains available for an explicit later resume. `codex exec`
        // exits after its primary turn, so it cannot consume a queued follow-up. Keep the response
        // model-visible there by falling back to passive delivery during the active close turn.
        let queue_delivery = policy.queue_input()
            && self
                .source_session
                .app_server_client_metadata()
                .await
                .client_name
                .as_deref()
                != Some("codex_exec");
        let mut communication = InterAgentCommunication::new(
            self.target,
            self.source,
            Vec::new(),
            message,
            /*trigger_turn*/
            queue_delivery || policy.final_response() == FinalResponseObservation::Wake,
        );
        communication.defer_to_next_turn = queue_delivery;
        crate::session::inter_agent_communication(
            &self.source_session,
            crate::session::new_submission_id(),
            communication,
            codex_protocol::turn_input::TurnStartOptions::default(),
        )
        .await;
        if queue_delivery {
            CloseAgentResponseDisposition::Queued
        } else {
            CloseAgentResponseDisposition::Delivered
        }
    }
}

async fn effective_history_contains_completion(
    source_session: &Session,
    source_reference: &str,
    target_references: &[String],
    payload: &str,
    expected_message: &str,
) -> bool {
    let active_turn_state = source_session
        .active_turn
        .lock()
        .await
        .as_ref()
        .map(|active_turn| Arc::clone(&active_turn.turn_state));
    if source_session
        .input_queue
        .has_pending_agent_completion(active_turn_state.as_deref(), expected_message)
        .await
    {
        return true;
    }
    let history = source_session.clone_history().await;
    let wait_call_ids = history
        .raw_items()
        .filter_map(wait_agent_call_id)
        .collect::<HashSet<_>>();
    history.raw_items().any(|item| {
        response_item_contains_completion(
            item,
            &wait_call_ids,
            source_reference,
            target_references,
            payload,
            expected_message,
        )
    })
}

fn wait_agent_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { name, call_id, .. }
        | ResponseItem::CustomToolCall { name, call_id, .. }
            if name == "wait_agent" =>
        {
            Some(call_id)
        }
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => None,
    }
}

fn response_item_contains_completion(
    item: &ResponseItem,
    wait_call_ids: &HashSet<&str>,
    source_reference: &str,
    target_references: &[String],
    payload: &str,
    expected_message: &str,
) -> bool {
    let output_contains_completion =
        |output: &codex_protocol::models::FunctionCallOutputPayload| {
            output.text_content().is_some_and(|text| {
                serde_json::from_str(text).is_ok_and(|value| {
                    value_contains_completed_status(&value, target_references, payload)
                })
            })
        };
    match item {
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => {
            matches!(
                content.as_slice(),
                [AgentMessageInputContent::InputText { text }]
                    if text == expected_message
            ) && target_references.iter().any(|target| target == author)
                && recipient == source_reference
        }
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            output,
            ..
        } if wait_call_ids.contains(call_id.as_str()) => output_contains_completion(output),
        ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } if wait_call_ids.contains(call_id.as_str()) => output_contains_completion(output),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Message { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => false,
    }
}

fn value_contains_completed_status(
    value: &serde_json::Value,
    target_references: &[String],
    payload: &str,
) -> bool {
    let expected_status = serde_json::json!({"completed": payload});
    value_contains_exact_status(value, target_references, &expected_status)
}

fn value_contains_exact_status(
    value: &serde_json::Value,
    target_references: &[String],
    expected_status: &serde_json::Value,
) -> bool {
    match value {
        serde_json::Value::Object(fields) => {
            target_references
                .iter()
                .any(|target| fields.get(target) == Some(expected_status))
                || fields.values().any(|value| {
                    value_contains_exact_status(value, target_references, expected_status)
                })
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| value_contains_exact_status(value, target_references, expected_status)),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => false,
    }
}

#[cfg(test)]
#[path = "close_response_tests.rs"]
mod tests;
