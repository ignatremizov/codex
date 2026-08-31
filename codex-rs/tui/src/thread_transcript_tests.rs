use super::RawReasoningVisibility;
use super::collab_agent_metadata_from_items;
use super::fallback_transcript_cell;
use super::refresh_collab_agent_metadata;
use super::thread_items_to_transcript_cells_with_metadata;
use super::thread_items_with_sources_to_transcript_cells;
use super::thread_to_transcript_cells;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use codex_app_server_protocol::CollabAgentRef;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use std::collections::HashMap;

fn thread_items_to_transcript_cells(
    thread_id: Option<ThreadId>,
    cwd: &codex_utils_absolute_path::AbsolutePathBuf,
    items: impl IntoIterator<Item = ThreadItem>,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&crate::legacy_core::config::Config>,
) -> super::TranscriptCells {
    let items = items.into_iter().collect::<Vec<_>>();
    let metadata = collab_agent_metadata_from_items(&items);
    thread_items_to_transcript_cells_with_metadata(
        thread_id,
        cwd,
        items,
        raw_reasoning_visibility,
        config,
        &metadata,
    )
}

#[test]
fn hydrated_user_message_renders_audio_markers_snapshot() {
    let cwd = test_path_buf("/tmp").abs();
    let cells = thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &cwd,
        [ThreadItem::UserMessage {
            id: "user-1".to_string(),
            client_id: None,
            content: vec![
                UserInput::Text {
                    text: "Please inspect the attachments.".to_string(),
                    text_elements: Vec::new(),
                },
                UserInput::Audio {
                    url: "https://example.com/one.wav".to_string(),
                },
                UserInput::LocalAudio {
                    path: test_path_buf("/tmp/two.wav"),
                },
            ],
        }],
        RawReasoningVisibility::Hidden,
        /*codex_home*/ None,
    );
    let rendered = cells
        .into_iter()
        .flat_map(|cell| cell.display_lines(/*width*/ 200))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(
        rendered,
        @r"
    › Please inspect the attachments.
      [audio]
      [audio]

    "
    );
}

#[test]
fn hydrated_user_message_preserves_canonical_turn_and_item_identity() {
    let cwd = test_path_buf("/tmp").abs();
    let cells = thread_items_with_sources_to_transcript_cells(
        /*thread_id*/ None,
        &cwd,
        [(
            Some("turn-1".to_string()),
            ThreadItem::UserMessage {
                id: "user-1".to_string(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: "prompt".to_string(),
                    text_elements: Vec::new(),
                }],
            },
        )],
        RawReasoningVisibility::Hidden,
        /*config*/ None,
        &HashMap::new(),
    );

    let source = cells[0]
        .as_any()
        .downcast_ref::<UserHistoryCell>()
        .and_then(|cell| cell.source.as_ref());
    assert_eq!(
        source,
        Some(&crate::history_cell::UserMessageSource {
            item_id: "user-1".to_string(),
            turn_id: "turn-1".to_string(),
        })
    );
}

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
            target_messages: None,
            queue_input: None,
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
fn compaction_decode_error_transcript_snapshot() {
    let item = ThreadItem::ContextCompaction {
        id: "compact-1".to_string(),
        summary: None,
        message: None,
        decode_error: Some("Selected model is at capacity.".to_string()),
        available_skills: Vec::new(),
    };
    let rendered = fallback_transcript_cell(&item)
        .expect("compaction error should render")
        .display_lines(/*width*/ 200)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(
        rendered,
        @"context compacted · prompt decoding failed: Selected model is at capacity."
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
        project_id: None,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        model: None,
        reasoning_effort: None,
        created_at: 1,
        updated_at: 2,
        recency_at: Some(2),
        status: ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp").abs(),
        cli_version: "0.0.0".to_string(),
        source: codex_app_server_protocol::SessionSource::Cli,
        can_accept_direct_input: None,
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
                    target_messages: Some(false),
                    queue_input: Some(false),
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
                    delivery: None,
                    questions: None,
                },
                ThreadItem::AgentMessage {
                    id: format!("msg_a_{}", ThreadId::new()),
                    text: format!(
                        "Agent message from `{child_thread_id}`:\n\nPlease confirm the boundary."
                    ),
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                },
                ThreadItem::AgentMessage {
                    id: format!("msg_c_{}", ThreadId::new()),
                    text: format!(
                        "Agent final answer from `{child_thread_id}`:\n\nFinished the review."
                    ),
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                    delivery: None,
                    questions: None,
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
    • Robie [explorer] sends:
      └ I found the relevant path.
    • Robie [explorer] sends:
      └ Please confirm the boundary.
    • Robie [explorer] completed (● visible):
      └ Finished the review.
    "
    );
}

#[test]
fn split_page_completion_merges_thread_wide_collab_metadata_snapshot() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let metadata_item = ThreadItem::CollabAgentToolCall {
        id: "spawn-1".to_string(),
        tool: CollabAgentTool::SpawnAgent,
        status: CollabAgentToolCallStatus::Completed,
        observe_commentary: Some(false),
        wake_on_completion: Some(false),
        target_messages: Some(false),
        queue_input: Some(false),
        sender_thread_id: parent_thread_id.to_string(),
        receiver_thread_ids: vec![child_thread_id.to_string()],
        receiver_agents: vec![CollabAgentRef {
            thread_id: child_thread_id.to_string(),
            agent_nickname: Some("Robie".to_string()),
            agent_role: Some("explorer".to_string()),
        }],
        prompt: Some("Inspect the change.".to_string()),
        model: Some("gpt-5.6-sol".to_string()),
        reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::High),
        agents_states: HashMap::new(),
    };
    let mut cells = thread_items_to_transcript_cells(
        Some(parent_thread_id),
        &test_path_buf("/tmp").abs(),
        [ThreadItem::AgentMessage {
            id: format!("msg_c_{}", ThreadId::new()),
            text: format!(
                "Agent final answer from `{child_thread_id}`:\n\nFinished the split-page review."
            ),
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
        }],
        RawReasoningVisibility::Hidden,
        /*codex_home*/ None,
    );
    let collab_agent_metadata = collab_agent_metadata_from_items([&metadata_item]);
    assert!(refresh_collab_agent_metadata(
        &mut cells,
        &collab_agent_metadata
    ));
    let mut renamed_metadata_item = metadata_item;
    let ThreadItem::CollabAgentToolCall {
        receiver_agents, ..
    } = &mut renamed_metadata_item
    else {
        panic!("expected collab agent metadata");
    };
    *receiver_agents = vec![CollabAgentRef {
        thread_id: child_thread_id.to_string(),
        agent_nickname: Some("Robie II".to_string()),
        agent_role: None,
    }];
    let renamed_collab_agent_metadata = collab_agent_metadata_from_items([&renamed_metadata_item]);
    assert!(refresh_collab_agent_metadata(
        &mut cells,
        &renamed_collab_agent_metadata
    ));
    let rendered = cells
        .into_iter()
        .flat_map(|cell| cell.transcript_lines(/*width*/ 200))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(
        rendered,
        @r"
    • Robie II [explorer] (gpt-5.6-sol high) completed (● visible):
      └ Finished the split-page review.
    "
    );
}
