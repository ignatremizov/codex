//! Enrich background response labels from structured collaboration history across loaded pages.

use super::TranscriptCells;
use crate::multi_agents::AgentMetadata;
use crate::multi_agents::CollabAgentHistoryCell;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
#[path = "agent_metadata_tests.rs"]
mod tests;

pub(crate) fn collab_agent_metadata_from_items<'a>(
    items: impl IntoIterator<Item = &'a ThreadItem>,
) -> HashMap<ThreadId, AgentMetadata> {
    let mut metadata = HashMap::<ThreadId, AgentMetadata>::new();
    for item in items {
        if let ThreadItem::CollabAgentToolCall {
            receiver_agents, ..
        } = item
        {
            for agent in receiver_agents {
                let Ok(thread_id) = ThreadId::from_string(&agent.thread_id) else {
                    continue;
                };
                let entry = metadata.entry(thread_id).or_default();
                if agent.agent_nickname.is_some() {
                    entry.agent_nickname.clone_from(&agent.agent_nickname);
                }
                if agent.agent_role.is_some() {
                    entry.agent_role.clone_from(&agent.agent_role);
                }
            }
        }
    }
    metadata
}

pub(crate) fn refresh_collab_agent_labels(
    cells: &mut TranscriptCells,
    metadata: &HashMap<ThreadId, AgentMetadata>,
) {
    for cell in cells {
        if let Some(updated) = cell
            .as_any()
            .downcast_ref::<CollabAgentHistoryCell>()
            .and_then(|cell| {
                cell.with_refreshed_agent_metadata(|thread_id| metadata.get(&thread_id).cloned())
            })
        {
            *cell = Arc::new(updated);
        }
    }
}
