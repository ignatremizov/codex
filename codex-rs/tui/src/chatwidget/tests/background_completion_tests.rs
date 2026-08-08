use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::sub_agent_completion_transcript;
use codex_protocol::protocol::sub_agent_completion_transcript_with_visibility;

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
    • /root/reviewer completed (visible):
      └ Finished reviewing the change.
    "
    );
}

#[tokio::test]
async fn background_completion_shows_parent_model_visibility() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let receiver_thread_id =
        ThreadId::from_string("019fc1b4-78ea-7481-97ac-ff423900cc6a").expect("valid thread");
    chat.set_collab_agent_metadata(
        receiver_thread_id,
        Some("Herschel".to_string()),
        Some("default".to_string()),
    );
    for model_visibility in [
        SubAgentCompletionModelVisibility::Visible,
        SubAgentCompletionModelVisibility::NotVisible,
    ] {
        let (id, text) = sub_agent_completion_transcript_with_visibility(
            &receiver_thread_id.to_string(),
            &AgentStatus::Completed(Some("Finished.".to_string())),
            model_visibility,
        )
        .expect("terminal status");
        chat.handle_server_notification(
            completion_notification(id.to_string(), text, MessagePhase::Commentary),
            /*replay_kind*/ None,
        );
    }

    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(
        rendered,
        @r"
    • Herschel [default] completed (visible):
      └ Finished.


    • Herschel [default] completed (not visible):
      └ Finished.
    "
    );
}

#[tokio::test]
async fn background_completion_moves_terminal_status_to_agent_title() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let receiver_thread_id =
        ThreadId::from_string("019fc1b4-78ea-7481-97ac-ff423900cc6a").expect("valid thread");
    chat.set_collab_agent_metadata(
        receiver_thread_id,
        Some("Herschel".to_string()),
        Some("default".to_string()),
    );
    for status in [
        AgentStatus::Completed(Some("Finished.".to_string())),
        AgentStatus::Errored("API failed.".to_string()),
        AgentStatus::Shutdown,
    ] {
        let (id, text) = sub_agent_completion_transcript(&receiver_thread_id.to_string(), &status)
            .expect("terminal status");
        chat.handle_server_notification(
            completion_notification(id.to_string(), text, MessagePhase::Commentary),
            /*replay_kind*/ None,
        );
    }
    let missing_thread_id =
        ThreadId::from_string("019fc1b4-78ea-7481-97ac-ff423900cc6b").expect("valid thread");
    let (id, text) =
        sub_agent_completion_transcript(&missing_thread_id.to_string(), &AgentStatus::NotFound)
            .expect("terminal status");
    chat.handle_server_notification(
        completion_notification(id.to_string(), text, MessagePhase::Commentary),
        /*replay_kind*/ None,
    );

    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(
        rendered,
        @r"
    • Herschel [default] completed (visible):
      └ Finished.


    • Herschel [default] errored (visible):
      └ API failed.


    • Herschel [default] shut down (visible)


    • 019fc1b4-78ea-7481-97ac-ff423900cc6b not found (visible)
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
                observe_commentary: None,
                wake_on_completion: None,
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
    assert!(rendered.contains("/root/reviewer completed (visible)"));
    assert!(rendered.contains("Finished waiting"));
    let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(normalized.matches(response).count(), 2);
}

#[tokio::test]
async fn background_completion_resolves_thread_id_from_cached_agent_metadata() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let receiver_thread_id =
        ThreadId::from_string("019fc1b4-78ea-7481-97ac-ff423900cc6a").expect("valid thread");
    chat.set_collab_agent_metadata(
        receiver_thread_id,
        Some("Herschel".to_string()),
        Some("default".to_string()),
    );
    let (id, text) = completed_item(&receiver_thread_id.to_string(), "Cinnamon");

    chat.handle_server_notification(
        completion_notification(id, text, MessagePhase::Commentary),
        /*replay_kind*/ None,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_snapshot!(
        lines_to_single_string(&cells[0]),
        @r"
    • Herschel [default] completed (visible):
      └ Cinnamon
    "
    );
}

#[tokio::test]
async fn replayed_spawn_and_send_input_preserve_metadata_for_background_completion() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let sender_thread_id = ThreadId::new();
    let receiver_thread_id =
        ThreadId::from_string("019fc1b4-78ea-7481-97ac-ff423900cc6a").expect("valid thread");
    let (completion_id, completion_text) =
        completed_item(&receiver_thread_id.to_string(), "Cinnamon");

    chat.replay_thread_item(
        AppServerThreadItem::CollabAgentToolCall {
            id: "spawn-1".to_string(),
            tool: AppServerCollabAgentTool::SpawnAgent,
            status: AppServerCollabAgentToolCallStatus::Completed,
            observe_commentary: Some(false),
            wake_on_completion: Some(false),
            sender_thread_id: sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_thread_id.to_string()],
            receiver_agents: vec![codex_app_server_protocol::CollabAgentRef {
                thread_id: receiver_thread_id.to_string(),
                agent_nickname: Some("Herschel".to_string()),
                agent_role: Some("default".to_string()),
            }],
            prompt: Some("Review the metadata presentation change.".to_string()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::from([(
                receiver_thread_id.to_string(),
                AppServerCollabAgentState {
                    status: AppServerCollabAgentStatus::PendingInit,
                    message: None,
                },
            )]),
        },
        "turn-1".to_string(),
        ReplayKind::ResumeInitialMessages,
    );
    chat.replay_thread_item(
        AppServerThreadItem::CollabAgentToolCall {
            id: "send-1".to_string(),
            tool: AppServerCollabAgentTool::SendInput,
            status: AppServerCollabAgentToolCallStatus::Completed,
            observe_commentary: Some(false),
            wake_on_completion: Some(false),
            sender_thread_id: sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_thread_id.to_string()],
            receiver_agents: vec![codex_app_server_protocol::CollabAgentRef {
                thread_id: receiver_thread_id.to_string(),
                agent_nickname: None,
                agent_role: None,
            }],
            prompt: Some("Give me one random ingredient.".to_string()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::from([(
                receiver_thread_id.to_string(),
                AppServerCollabAgentState {
                    status: AppServerCollabAgentStatus::Running,
                    message: None,
                },
            )]),
        },
        "turn-2".to_string(),
        ReplayKind::ResumeInitialMessages,
    );
    chat.replay_thread_item(
        AppServerThreadItem::AgentMessage {
            id: completion_id,
            text: completion_text,
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
        },
        "turn-3".to_string(),
        ReplayKind::ResumeInitialMessages,
    );

    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(
        rendered,
    @r"
    • Spawned Herschel [default] (no commentary · no wake on completion)
      └ Review the metadata presentation change.


    • Sent input to Herschel [default] (no commentary · no wake on completion)
      └ Give me one random ingredient.


    • Herschel [default] completed (visible):
      └ Cinnamon
    "
    );
}
