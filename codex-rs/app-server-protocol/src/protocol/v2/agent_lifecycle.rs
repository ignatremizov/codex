use serde::Deserialize;
use serde::Serialize;

use crate::JsonSchema;
use crate::TS;

/// Ownership operation, distinct from the tool used to request it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CollabAgentLifecycleAction {
    Detach,
    Reattach,
}

/// Root involved in a detach or reattachment; this metadata grants no control authority.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollabAgentLifecycle {
    pub action: CollabAgentLifecycleAction,
    /// Source root for detachment, destination root for reattachment.
    pub root_thread_id: String,
}

impl From<codex_protocol::CollabAgentLifecycle> for CollabAgentLifecycle {
    fn from(value: codex_protocol::CollabAgentLifecycle) -> Self {
        Self {
            action: match value.action {
                codex_protocol::CollabAgentLifecycleAction::Detach => {
                    CollabAgentLifecycleAction::Detach
                }
                codex_protocol::CollabAgentLifecycleAction::Reattach => {
                    CollabAgentLifecycleAction::Reattach
                }
            },
            root_thread_id: value.root_thread_id.to_string(),
        }
    }
}
