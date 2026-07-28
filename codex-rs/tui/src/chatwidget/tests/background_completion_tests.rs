use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::sub_agent_completion_transcript;

use super::*;

fn completed_item(agent_reference: &str, response: &str) -> (String, String) {
    let (id, text) = sub_agent_completion_transcript(
        agent_reference,
        &AgentStatus::Completed(Some(response.to_string())),
    )
    .expect("terminal status");
    (id.to_string(), text)
}

fn completion_notification(id: String, text: String, phase: MessagePhase) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        completed_at_ms: 0,
        item: AppServerThreadItem::AgentMessage {
            id,
            text,
            phase: Some(phase),
            memory_citation: None,
        },
    })
}

#[tokio::test]
async fn completion_requires_canonical_phase_and_replays_with_wait_rendering() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let (id, text) = completed_item("/root/reviewer", "Finished reviewing the change.");

    chat.handle_server_notification(
        completion_notification(id.clone(), text.clone(), MessagePhase::FinalAnswer),
        /*replay_kind*/ None,
    );
    chat.replay_thread_item(
        AppServerThreadItem::AgentMessage {
            id,
            text,
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
        },
        "turn-1".to_string(),
        ReplayKind::ResumeInitialMessages,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 2);
    let commentary = lines_to_single_string(&cells[0]);
    assert!(commentary.contains("Agent final answer from /root/reviewer"));
    assert!(!commentary.contains("Agent finished"));
    assert_snapshot!(
        lines_to_single_string(&cells[1]),
        @r"
    • Agent finished
      └ /root/reviewer: Completed - Finished reviewing the change.
    "
    );
}

#[tokio::test]
async fn background_completion_and_later_wait_render_as_distinct_rows() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let sender_thread_id = ThreadId::new();
    let receiver_thread_id = ThreadId::new();
    let response = "Finished reviewing the change.";
    let (completion_id, completion_text) = completed_item("/root/reviewer", response);

    chat.handle_server_notification(
        completion_notification(completion_id, completion_text, MessagePhase::Commentary),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
            item: AppServerThreadItem::CollabAgentToolCall {
                id: "wait-1".to_string(),
                tool: AppServerCollabAgentTool::Wait,
                status: AppServerCollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![receiver_thread_id.to_string()],
                receiver_agents: Vec::new(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::from([(
                    receiver_thread_id.to_string(),
                    AppServerCollabAgentState {
                        status: AppServerCollabAgentStatus::Completed,
                        message: Some(response.to_string()),
                    },
                )]),
            },
        }),
        /*replay_kind*/ None,
    );

    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Agent finished"));
    assert!(rendered.contains("Finished waiting"));
    let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(normalized.matches(response).count(), 2);
}
