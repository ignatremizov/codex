use super::*;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::CollabAgentRef;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

fn lifecycle_item(
    action: CollabAgentLifecycleAction,
    status: CollabAgentToolCallStatus,
    agent_status: CollabAgentStatus,
) -> ThreadItem {
    let target = ThreadId::from_string("00000000-0000-7000-8000-000000000038").expect("target id");
    let root = ThreadId::from_string("00000000-0000-7000-8000-000000000001").expect("root id");
    let task_path = if action == CollabAgentLifecycleAction::Reattach
        && status == CollabAgentToolCallStatus::Completed
    {
        "/root/self-identity-reattached"
    } else {
        "/root/self-identity-lifecycle"
    };
    let observation = match action {
        CollabAgentLifecycleAction::Detach => None,
        CollabAgentLifecycleAction::Reattach => Some(false),
    };
    ThreadItem::CollabAgentToolCall {
        lifecycle: Some(CollabAgentLifecycle {
            action,
            root_thread_id: root.to_string(),
        }),
        id: "ownership-change".into(),
        tool: match action {
            CollabAgentLifecycleAction::Detach => CollabAgentTool::CloseAgent,
            CollabAgentLifecycleAction::Reattach => CollabAgentTool::ResumeAgent,
        },
        status,
        observe_commentary: observation,
        wake_on_completion: observation,
        target_messages: observation,
        queue_input: observation,
        input_batch: None,
        mailbox_input: None,
        sender_thread_id: root.to_string(),
        receiver_thread_ids: vec![target.to_string()],
        receiver_agents: vec![CollabAgentRef {
            thread_id: target.to_string(),
            agent_ref: Some("38".into()),
            task_path: Some(task_path.into()),
            agent_nickname: Some("Mencius".into()),
            agent_role: None,
        }],
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::from([(
            target.to_string(),
            CollabAgentState {
                status: agent_status,
                message: None,
            },
        )]),
    }
}

fn render(
    action: CollabAgentLifecycleAction,
    status: CollabAgentToolCallStatus,
    agent_status: CollabAgentStatus,
) -> String {
    render_item(&lifecycle_item(action, status, agent_status))
}

fn render_item(item: &ThreadItem) -> String {
    let root = ThreadId::from_string("00000000-0000-7000-8000-000000000001").expect("root id");
    let rendered = tool_call_history_cell(
        item,
        /*cached_spawn_request*/ None,
        /*agent_prompt_preview_lines*/ 3,
        /*agent_response_preview_lines*/ 3,
        |id| {
            if id == root {
                AgentMetadata {
                    agent_nickname: Some("Main".into()),
                    agent_role: Some("default".into()),
                    ..Default::default()
                }
            } else {
                // Later aliases must not rewrite a historical operation's captured identity.
                AgentMetadata {
                    agent_nickname: Some("Later nickname".into()),
                    agent_ref: Some("99".into()),
                    task_path: AgentTaskPath::Known(Some("/root/later".into())),
                    ..Default::default()
                }
            }
        },
    )
    .expect("ownership history cell")
    .display_lines(/*width*/ 160)
    .iter()
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::current_dir().expect("cwd"))
            .expect("absolute cwd");
    let replayed = crate::thread_transcript::thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &cwd,
        [item.clone()],
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        /*config*/ None,
    )
    .iter()
    .flat_map(|cell| cell.display_lines(/*width*/ 160))
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    assert_eq!(rendered, replayed);
    rendered
}

#[test]
fn ownership_round_trip_keeps_identity_action_and_readiness_separate() {
    let text = [
        render(
            CollabAgentLifecycleAction::Detach,
            CollabAgentToolCallStatus::InProgress,
            CollabAgentStatus::Completed,
        ),
        render(
            CollabAgentLifecycleAction::Detach,
            CollabAgentToolCallStatus::Completed,
            CollabAgentStatus::Shutdown,
        ),
        render(
            CollabAgentLifecycleAction::Reattach,
            CollabAgentToolCallStatus::InProgress,
            CollabAgentStatus::PendingInit,
        ),
        render(
            CollabAgentLifecycleAction::Reattach,
            CollabAgentToolCallStatus::Completed,
            CollabAgentStatus::PendingInit,
        ),
        render(
            CollabAgentLifecycleAction::Reattach,
            CollabAgentToolCallStatus::Completed,
            CollabAgentStatus::Completed,
        ),
    ]
    .join("\n");
    insta::assert_snapshot!(text, @r"
    • Detaching Mencius (38) from Main
    • Detached Mencius (38) from Main
      └ Resume 00000000-0000-7000-8000-000000000038 in another TUI to continue independently.
    • Reattaching Mencius (38) to Main
      └ (no commentary · no wake on completion)
    • Reattached Mencius (38) to Main
      └ Task: /root/self-identity-reattached
        Initializing
        (no commentary · no wake on completion)
    • Reattached Mencius (38) to Main
      └ Task: /root/self-identity-reattached
        Idle
        (no commentary · no wake on completion)
    ");
}

#[test]
fn unsuccessful_ownership_changes_do_not_report_success() {
    let text = [
        render(
            CollabAgentLifecycleAction::Detach,
            CollabAgentToolCallStatus::Failed,
            CollabAgentStatus::Running,
        ),
        render(
            CollabAgentLifecycleAction::Reattach,
            CollabAgentToolCallStatus::Interrupted,
            CollabAgentStatus::Running,
        ),
    ]
    .join("\n");
    insta::assert_snapshot!(text, @r"
    • Failed to detach Mencius (38) from Main
    • Reattachment interrupted: Mencius (38) to Main
      └ (no commentary · no wake on completion)
    ");
}

#[test]
fn unknown_caller_alias_uses_uuid_until_reattachment_assigns_one() {
    let mut item = lifecycle_item(
        CollabAgentLifecycleAction::Reattach,
        CollabAgentToolCallStatus::InProgress,
        CollabAgentStatus::PendingInit,
    );
    if let ThreadItem::CollabAgentToolCall {
        receiver_agents, ..
    } = &mut item
    {
        let receiver = &mut receiver_agents[0];
        receiver.agent_nickname = None;
        receiver.agent_ref = None;
        receiver.task_path = None;
    }
    insta::assert_snapshot!(render_item(&item), @r"
    • Reattaching 00000000-0000-7000-8000-000000000038 to Main
      └ (no commentary · no wake on completion)
    ");
}
