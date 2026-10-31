//! Ownership changes keep caller-root identity and lifecycle state distinct.

use super::*;
use codex_app_server_protocol::CollabAgentLifecycle;
use codex_app_server_protocol::CollabAgentLifecycleAction;

pub(super) fn history_cell(
    item: &ThreadItem,
    lifecycle: &CollabAgentLifecycle,
    response_observation: V1ResponseObservation,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> Option<CollabAgentHistoryCell> {
    let ThreadItem::CollabAgentToolCall {
        tool,
        status,
        receiver_thread_ids,
        receiver_agents,
        agents_states,
        ..
    } = item
    else {
        return None;
    };
    let expected_tool = match lifecycle.action {
        CollabAgentLifecycleAction::Detach => CollabAgentTool::CloseAgent,
        CollabAgentLifecycleAction::Reattach => CollabAgentTool::ResumeAgent,
    };
    if *tool != expected_tool {
        return None;
    }
    let target_id = receiver_thread_ids
        .first()
        .and_then(|id| parse_thread_id(id))?;
    let root_id = parse_thread_id(&lifecycle.root_thread_id)?;
    let mut metadata = agent_metadata(target_id);
    // Captured identity belongs to this operation, even when later aliases have changed.
    if let Some(receiver) = receiver_agents
        .iter()
        .find(|receiver| receiver.thread_id == target_id.to_string())
    {
        metadata.agent_nickname.clone_from(&receiver.agent_nickname);
        metadata.agent_ref.clone_from(&receiver.agent_ref);
        metadata.agent_role.clone_from(&receiver.agent_role);
        metadata.task_path = AgentTaskPath::Known(receiver.task_path.clone());
    }
    let (verb, relation) = match (lifecycle.action, status) {
        (CollabAgentLifecycleAction::Detach, CollabAgentToolCallStatus::InProgress) => {
            ("Detaching", " from ")
        }
        (CollabAgentLifecycleAction::Detach, CollabAgentToolCallStatus::Completed) => {
            ("Detached", " from ")
        }
        (CollabAgentLifecycleAction::Detach, CollabAgentToolCallStatus::Failed) => {
            ("Failed to detach", " from ")
        }
        (CollabAgentLifecycleAction::Detach, CollabAgentToolCallStatus::Interrupted) => {
            ("Detachment interrupted:", " from ")
        }
        (CollabAgentLifecycleAction::Reattach, CollabAgentToolCallStatus::InProgress) => {
            ("Reattaching", " to ")
        }
        (CollabAgentLifecycleAction::Reattach, CollabAgentToolCallStatus::Completed) => {
            ("Reattached", " to ")
        }
        (CollabAgentLifecycleAction::Reattach, CollabAgentToolCallStatus::Failed) => {
            ("Failed to reattach", " to ")
        }
        (CollabAgentLifecycleAction::Reattach, CollabAgentToolCallStatus::Interrupted) => {
            ("Reattachment interrupted:", " to ")
        }
    };
    let mut title = title_with_agent(
        verb,
        AgentLabel {
            task_path: None,
            spawn_request: None,
            ..agent_label(target_id, &metadata)
        },
        /*spawn_request*/ None,
    );
    title.spans.push(relation.bold());
    let root = agent_metadata(root_id);
    title.spans.extend(agent_label_spans(AgentLabel {
        nickname: root
            .agent_nickname
            .as_deref()
            .or(Some(codex_protocol::MAIN_AGENT_NICKNAME)),
        role: None,
        agent_ref: None,
        task_path: None,
        thread_id: Some(root_id),
        spawn_request: None,
    }));
    let mut details = Vec::new();
    match (lifecycle.action, status) {
        (CollabAgentLifecycleAction::Detach, CollabAgentToolCallStatus::Completed) => {
            details.push(
                format!("Resume {target_id} in another TUI to continue independently.").into(),
            );
        }
        (CollabAgentLifecycleAction::Reattach, CollabAgentToolCallStatus::Completed) => {
            if let AgentTaskPath::Known(Some(task_path)) = &metadata.task_path {
                details.push(format!("Task: {}", task_path.escape_debug()).into());
            }
            let state = first_agent_state(receiver_thread_ids, agents_states);
            details.push(match state.map(|state| &state.status) {
                Some(CollabAgentStatus::PendingInit) => "Initializing".into(),
                Some(CollabAgentStatus::Completed) => "Idle".into(),
                Some(
                    CollabAgentStatus::Running
                    | CollabAgentStatus::Interrupted
                    | CollabAgentStatus::Errored
                    | CollabAgentStatus::Shutdown
                    | CollabAgentStatus::NotFound,
                )
                | None => status_summary_line(state, "Agent reattachment failed"),
            });
        }
        (
            CollabAgentLifecycleAction::Detach | CollabAgentLifecycleAction::Reattach,
            CollabAgentToolCallStatus::Failed | CollabAgentToolCallStatus::Interrupted,
        ) => {
            if let Some(state) = first_agent_state(receiver_thread_ids, agents_states)
                && state.status == CollabAgentStatus::Errored
            {
                details.push(status_summary_line(Some(state), "Agent operation failed"));
            }
        }
        (
            CollabAgentLifecycleAction::Detach | CollabAgentLifecycleAction::Reattach,
            CollabAgentToolCallStatus::InProgress,
        ) => {}
    }
    if let Some(label) = response_observation_label(response_observation) {
        details.push(label.into());
    }
    Some(collab_event(title, fixed_details(details)))
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
