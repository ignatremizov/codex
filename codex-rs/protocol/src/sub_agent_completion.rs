//! Durable identity and terminal status for background subagent completions.

use crate::ResponseItemId;
use crate::items::AgentMessageContent;
use crate::items::AgentMessageItem;
use crate::models::MessagePhase;
use crate::protocol::AgentStatus;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

const SUB_AGENT_COMPLETION_ID_PREFIX: &str = "msg";
const SUB_AGENT_COMPLETION_CONTEXT_ID_PREFIX: &str = "msg_x";
const SUB_AGENT_COMPLETION_TRANSCRIPT_PREFIX: &str = "Agent final answer from `";
const SUB_AGENT_COMPLETION_TRANSCRIPT_SEPARATOR: &str = "`:\n\n";

/// Terminal status encoded into an internally generated subagent-completion response-item ID.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubAgentCompletionStatus {
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

impl SubAgentCompletionStatus {
    fn from_agent_status(status: &AgentStatus) -> Option<Self> {
        match status {
            AgentStatus::Completed(_) => Some(Self::Completed),
            AgentStatus::Errored(_) => Some(Self::Errored),
            AgentStatus::Shutdown => Some(Self::Shutdown),
            AgentStatus::NotFound => Some(Self::NotFound),
            AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => None,
        }
    }

    fn as_id_segment(self) -> &'static str {
        match self {
            Self::Completed => "c",
            Self::Errored => "e",
            Self::Shutdown => "s",
            Self::NotFound => "n",
        }
    }

    fn from_id_segment(segment: &str) -> Option<Self> {
        match segment {
            "c" => Some(Self::Completed),
            "e" => Some(Self::Errored),
            "s" => Some(Self::Shutdown),
            "n" => Some(Self::NotFound),
            _ => None,
        }
    }
}

/// Core-authored provenance for a visible background subagent completion.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
pub struct SubAgentCompletionMetadata {
    agent_reference: String,
    status: SubAgentCompletionStatus,
}

fn new_sub_agent_completion_response_item_id(status: SubAgentCompletionStatus) -> ResponseItemId {
    let status = status.as_id_segment();
    ResponseItemId::new(&format!("{SUB_AGENT_COMPLETION_ID_PREFIX}_{status}"))
}

/// Creates the model-context item ID paired with a terminal presentation token.
///
/// V1 notification envelopes are serialized as user-role `Message` input, while V2 inter-agent
/// messages may retain their richer rollout form. Both use this provider-compatible `msg`
/// identity, paired with rollout delivery metadata, so exact rollback can preserve core-authored
/// completion context without trusting message text.
pub fn new_sub_agent_completion_context_response_item_id() -> ResponseItemId {
    ResponseItemId::new(SUB_AGENT_COMPLETION_CONTEXT_ID_PREFIX)
}

/// Returns whether a response item belongs to a completion context delivery.
pub fn is_sub_agent_completion_context_response_item_id(id: &str) -> bool {
    has_uuid_v7_suffix(id, SUB_AGENT_COMPLETION_CONTEXT_ID_PREFIX)
}

/// Returns the terminal status encoded in a canonical background-completion item ID.
pub fn sub_agent_completion_status_from_response_item_id(
    id: &str,
) -> Option<SubAgentCompletionStatus> {
    let suffix = id
        .strip_prefix(SUB_AGENT_COMPLETION_ID_PREFIX)?
        .strip_prefix('_')?;
    let (status, unique_suffix) = suffix.split_once('_')?;
    let status = SubAgentCompletionStatus::from_id_segment(status)?;
    has_uuid_v7(unique_suffix).then_some(status)
}

fn has_uuid_v7_suffix(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('_'))
        .is_some_and(has_uuid_v7)
}

fn has_uuid_v7(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .ok()
        .is_some_and(|uuid| uuid.get_version() == Some(uuid::Version::SortRand))
}

impl AgentMessageItem {
    /// Returns whether this item carries the reserved background-completion identity.
    pub fn has_sub_agent_completion_identity(&self) -> bool {
        let Some(metadata) = self.sub_agent_completion.as_ref() else {
            return false;
        };
        let Some(status) = sub_agent_completion_status_from_response_item_id(&self.id) else {
            return false;
        };
        let [AgentMessageContent::Text { text }] = self.content.as_slice() else {
            return false;
        };
        let Some((agent_reference, _)) = sub_agent_completion_transcript_parts(text) else {
            return false;
        };
        self.phase == Some(MessagePhase::Commentary)
            && status == metadata.status
            && agent_reference == metadata.agent_reference
    }
}

/// Prevents an ordinary provider-authored agent message from using the reserved completion ID.
pub fn ordinary_agent_message_response_item_id(id: &str) -> String {
    if sub_agent_completion_status_from_response_item_id(id).is_some() {
        format!("agent_{id}")
    } else {
        id.to_string()
    }
}

/// Builds the canonical parent-thread item ID and transcript text for a terminal status.
pub fn sub_agent_completion_transcript(
    agent_reference: &str,
    status: &AgentStatus,
) -> Option<(ResponseItemId, String)> {
    let completion_status = SubAgentCompletionStatus::from_agent_status(status)?;
    let payload = match status {
        AgentStatus::Completed(message) => message.as_deref().unwrap_or_default(),
        AgentStatus::Errored(error) => error,
        AgentStatus::Shutdown | AgentStatus::NotFound => "",
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some((
        new_sub_agent_completion_response_item_id(completion_status),
        sub_agent_completion_transcript_text(agent_reference, payload),
    ))
}

/// Builds the canonical, provenance-bearing parent-thread item for a terminal status.
pub fn sub_agent_completion_item(
    agent_reference: &str,
    status: &AgentStatus,
) -> Option<AgentMessageItem> {
    let completion_status = SubAgentCompletionStatus::from_agent_status(status)?;
    let (id, text) = sub_agent_completion_transcript(agent_reference, status)?;
    Some(AgentMessageItem {
        id: id.to_string(),
        content: vec![AgentMessageContent::Text { text }],
        phase: Some(MessagePhase::Commentary),
        memory_citation: None,
        sub_agent_completion: Some(SubAgentCompletionMetadata {
            agent_reference: agent_reference.to_string(),
            status: completion_status,
        }),
    })
}

fn sub_agent_completion_transcript_text(agent_reference: &str, payload: &str) -> String {
    format!(
        "{SUB_AGENT_COMPLETION_TRANSCRIPT_PREFIX}{agent_reference}{SUB_AGENT_COMPLETION_TRANSCRIPT_SEPARATOR}{payload}"
    )
}

/// Parses a canonical parent-thread completion transcript into its agent reference and payload.
pub fn sub_agent_completion_transcript_parts(text: &str) -> Option<(&str, &str)> {
    text.strip_prefix(SUB_AGENT_COMPLETION_TRANSCRIPT_PREFIX)?
        .split_once(SUB_AGENT_COMPLETION_TRANSCRIPT_SEPARATOR)
}

#[cfg(test)]
#[path = "sub_agent_completion_tests.rs"]
mod tests;
