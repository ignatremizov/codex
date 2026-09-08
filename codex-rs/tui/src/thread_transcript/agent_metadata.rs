//! Enrich background response labels from structured collaboration history across loaded pages.

use super::TranscriptCells;
use crate::multi_agents::AgentMetadata;
use crate::multi_agents::AgentTaskPath;
use crate::multi_agents::CollabAgentHistoryCell;
use crate::multi_agents::spawn_request_summary;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserAgentControlStatus;
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
        match item {
            ThreadItem::CollabAgentToolCall {
                receiver_agents,
                receiver_thread_ids,
                ..
            } => {
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
                    if let Some(task_path) = &agent.task_path {
                        entry.task_path = AgentTaskPath::Known(Some(task_path.clone()));
                    }
                    if let Some(request) = spawn_request_summary(item) {
                        entry.spawn_request = Some(request);
                    }
                }
                if let Some(request) = spawn_request_summary(item) {
                    for thread_id in receiver_thread_ids {
                        if let Ok(thread_id) = ThreadId::from_string(thread_id) {
                            metadata.entry(thread_id).or_default().spawn_request =
                                Some(request.clone());
                        }
                    }
                }
            }
            ThreadItem::UserAgentControl {
                target_thread_id: Some(thread_id),
                nickname,
                role,
                task_path,
                task_path_mapping,
                status: UserAgentControlStatus::Succeeded,
                ..
            } => {
                let Ok(thread_id) = ThreadId::from_string(thread_id) else {
                    continue;
                };
                let entry = metadata.entry(thread_id).or_default();
                if nickname.is_some() {
                    entry.agent_nickname.clone_from(nickname);
                }
                if role.is_some() {
                    entry.agent_role.clone_from(role);
                }
                if let Some(request) = spawn_request_summary(item) {
                    entry.spawn_request = Some(request);
                }
                if let Some(task_path) = task_path {
                    entry.task_path = AgentTaskPath::Known(Some(task_path.clone()));
                }
                for mapping in task_path_mapping {
                    if let Ok(thread_id) = ThreadId::from_string(&mapping.thread_id) {
                        metadata.entry(thread_id).or_default().task_path =
                            AgentTaskPath::Known(mapping.task_path.clone());
                    }
                }
            }
            _ => {}
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
