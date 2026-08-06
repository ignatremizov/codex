use super::RawReasoningVisibility;
use super::fallback_transcript_cell;
use super::thread_to_transcript_cells;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::CollabAgentRef;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use std::collections::HashMap;

#[test]
fn collab_response_observation_transcript_snapshot() {
    let rendered = [
        (Some(false), Some(true)),
        (Some(true), Some(false)),
        (None, None),
    ]
    .into_iter()
    .map(|(observe_commentary, wake_on_completion)| {
        let item = ThreadItem::CollabAgentToolCall {
            id: "call-1".to_string(),
            tool: CollabAgentTool::SendInput,
            status: CollabAgentToolCallStatus::Completed,
            observe_commentary,
            wake_on_completion,
            sender_thread_id: ThreadId::new().to_string(),
            receiver_thread_ids: Vec::new(),
            receiver_agents: Vec::new(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        fallback_transcript_cell(&item)
            .expect("collab tool call should render")
            .display_lines(/*width*/ 200)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })
    .collect::<Vec<_>>()
    .join("\n");

    insta::assert_snapshot!(
        rendered,
        @r"
    agent tool: SendInput · Completed · no commentary · wake on completion
    agent tool: SendInput · Completed · receive commentary · no wake on completion
    agent tool: SendInput · Completed
    "
    );
}

#[test]
fn full_transcript_renders_collab_messages_with_persisted_agent_metadata_snapshot() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let thread = Thread {
        id: parent_thread_id.to_string(),
        extra: None,
        session_id: parent_thread_id.to_string(),
        forked_from_id: None,
        parent_thread_id: None,
        preview: "preview".to_string(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        created_at: 1,
        updated_at: 2,
        recency_at: Some(2),
        status: ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp").abs(),
        cli_version: "0.0.0".to_string(),
        source: codex_app_server_protocol::SessionSource::Cli,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: vec![Turn {
            id: "turn-1".to_string(),
            items_view: TurnItemsView::Full,
            items: vec![
                ThreadItem::CollabAgentToolCall {
                    id: "spawn-1".to_string(),
                    tool: CollabAgentTool::SpawnAgent,
                    status: CollabAgentToolCallStatus::Completed,
                    observe_commentary: Some(true),
                    wake_on_completion: Some(false),
                    sender_thread_id: parent_thread_id.to_string(),
                    receiver_thread_ids: vec![child_thread_id.to_string()],
                    receiver_agents: vec![CollabAgentRef {
                        thread_id: child_thread_id.to_string(),
                        agent_nickname: Some("Robie".to_string()),
                        agent_role: Some("explorer".to_string()),
                    }],
                    prompt: Some("Inspect the change.".to_string()),
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                },
                ThreadItem::AgentMessage {
                    id: "commentary-1".to_string(),
                    text: format!(
                        "Agent commentary from `{child_thread_id}`:\n\nI found the relevant path."
                    ),
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                },
                ThreadItem::AgentMessage {
                    id: format!("msg_c_{}", ThreadId::new()),
                    text: format!(
                        "Agent final answer from `{child_thread_id}`:\n\nFinished the review."
                    ),
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                },
            ],
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }],
    };

    let cells = thread_to_transcript_cells(
        thread,
        RawReasoningVisibility::Hidden,
        /*codex_home*/ None,
    );
    let rendered = cells
        .iter()
        .skip(1)
        .flat_map(|cell| cell.transcript_lines(/*width*/ 200))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(
        rendered,
        @r"
    • Agent message from Robie [explorer]
      └ I found the relevant path.
    • Agent finished
      └ Robie [explorer]: Completed - Finished the review.
    "
    );
}
