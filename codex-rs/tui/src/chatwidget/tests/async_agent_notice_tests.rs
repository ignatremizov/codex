use super::*;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::sub_agent_completion_transcript_with_visibility;
use pretty_assertions::assert_eq;

fn message(id: String, text: String, phase: MessagePhase) -> AppServerThreadItem {
    AppServerThreadItem::AgentMessage {
        id,
        text,
        phase: Some(phase),
        attribution: None,
        input: None,
        memory_citation: None,
        delivery: None,
        questions: None,
    }
}

fn completion(visibility: SubAgentCompletionModelVisibility) -> AppServerThreadItem {
    let (id, text) = sub_agent_completion_transcript_with_visibility(
        "/root/reviewer",
        &AgentStatus::Completed(Some("Finished review.".to_string())),
        visibility,
    )
    .expect("completion");
    message(id.to_string(), text, MessagePhase::Commentary)
}

fn deliver(chat: &mut ChatWidget, item: AppServerThreadItem) {
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: chat
                .thread_id
                .map_or_else(|| "thread-1".to_string(), |id| id.to_string()),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item,
        }),
        /*replay_kind*/ None,
    );
}

// Observe consolidation and notice insertion order, including any premature partial
// consolidation. Streaming continuation cells are owned by the consolidation event.
fn output(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::ConsolidateAgentMessage { source, .. } => {
                Some(format!("answer: {}", source.trim_end()))
            }
            AppEvent::InsertHistoryCell(cell)
                if cell
                    .as_any()
                    .is::<crate::multi_agents::CollabAgentHistoryCell>()
                    || cell.as_any().is::<history_cell::AgentInputHistoryCell>() =>
            {
                Some(
                    cell.display_lines(/*width*/ 200)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn asynchronous_completion_never_splits_the_authored_answer() {
    for visibility in [
        SubAgentCompletionModelVisibility::Visible,
        SubAgentCompletionModelVisibility::NotVisible,
    ] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.on_agent_message_delta("UUID fallback and".to_string());
        deliver(&mut chat, completion(visibility));
        assert!(chat.stream_controller.is_some());
        assert_eq!(output(&mut rx), Vec::<String>::new());
        chat.on_agent_message_delta(" complete guidance.".to_string());
        deliver(
            &mut chat,
            message(
                "main-final".to_string(),
                "UUID fallback and complete guidance.".to_string(),
                MessagePhase::FinalAnswer,
            ),
        );
        let rendered = output(&mut rx);
        let marker = match visibility {
            SubAgentCompletionModelVisibility::Visible => "● visible",
            SubAgentCompletionModelVisibility::NotVisible => "○ not visible",
        };
        assert_eq!(
            rendered,
            vec![
                "answer: UUID fallback and complete guidance.".to_string(),
                format!("• /root/reviewer completed: ({marker})\n  └ Finished review."),
            ],
        );
        assert!(chat.interrupts.is_empty());
    }
}

#[tokio::test]
async fn notices_keep_fifo_and_distinct_authored_messages_are_not_suppressed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let reviewer = ThreadId::new();
    chat.set_collab_agent_metadata(
        reviewer,
        Some("Reviewer".to_string()),
        /*agent_role*/ None,
    );
    chat.on_agent_message_delta("Same answer.".to_string());
    deliver(
        &mut chat,
        message(
            "commentary".to_string(),
            format!("Agent commentary from `{reviewer}`:\n\nWorking."),
            MessagePhase::Commentary,
        ),
    );
    deliver(
        &mut chat,
        completion(SubAgentCompletionModelVisibility::NotVisible),
    );
    assert!(output(&mut rx).is_empty());
    for id in ["first-real-message", "second-real-message"] {
        deliver(
            &mut chat,
            message(
                id.to_string(),
                "Same answer.".to_string(),
                MessagePhase::FinalAnswer,
            ),
        );
    }
    insta::assert_snapshot!(output(&mut rx).join("\n"), @r"
    answer: Same answer.
    • Reviewer commentary:
      └ Working.
    • /root/reviewer completed: (○ not visible)
      └ Finished review.
    answer: Same answer.
    ");
}

#[tokio::test]
async fn termination_consolidates_partial_answer_and_drains_only_notices() {
    enum Termination {
        Interrupt,
        Error,
        Overloaded,
        CompleteWithoutItem,
    }
    for termination in [
        Termination::Interrupt,
        Termination::Error,
        Termination::Overloaded,
        Termination::CompleteWithoutItem,
    ] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.on_agent_message_delta("Partial answer".to_string());
        chat.interrupts
            .push_user_input(codex_app_server_protocol::ToolRequestUserInputParams {
                thread_id: "thread-1".to_string(),
                item_id: "pending-prompt".to_string(),
                turn_id: "turn-1".to_string(),
                questions: Vec::new(),
                is_blocking: true,
                auto_resolution_ms: None,
            });
        deliver(
            &mut chat,
            completion(SubAgentCompletionModelVisibility::NotVisible),
        );
        match termination {
            Termination::Interrupt => chat.on_interrupted_turn(TurnAbortReason::Interrupted),
            Termination::Error => {
                chat.handle_non_retry_error("failure".to_string(), /*codex_error_info*/ None)
            }
            Termination::Overloaded => chat.on_server_overloaded_error("overloaded".to_string()),
            Termination::CompleteWithoutItem => chat.on_task_complete(
                /*last_agent_message*/ None, /*duration_ms*/ None,
                /*from_replay*/ false,
            ),
        }
        assert_eq!(
            output(&mut rx),
            vec![
                "answer: Partial answer".to_string(),
                "• /root/reviewer completed: (○ not visible)\n  └ Finished review.".to_string(),
            ]
        );
        assert!(chat.interrupts.has_pending_prompt());
        assert!(chat.stream_controller.is_none());
        chat.finalize_turn();
        assert!(output(&mut rx).is_empty(), "cleanup must not emit twice");
        deliver(
            &mut chat,
            completion(SubAgentCompletionModelVisibility::NotVisible),
        );
        assert_eq!(
            output(&mut rx),
            vec!["• /root/reviewer completed: (○ not visible)\n  └ Finished review.".to_string()],
            "retained prompts must not defer an idle notice",
        );
        assert!(chat.interrupts.has_pending_prompt());
    }
}

#[tokio::test]
async fn replayed_notices_remain_immediate() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_agent_message_delta("Live text before replay.".to_string());
    chat.replay_thread_item(
        completion(SubAgentCompletionModelVisibility::NotVisible),
        "replayed-turn".to_string(),
        ReplayKind::ResumeInitialMessages,
    );
    assert_eq!(
        output(&mut rx),
        vec![
            "answer: Live text before replay.".to_string(),
            "• /root/reviewer completed: (○ not visible)\n  └ Finished review.".to_string(),
        ]
    );
    assert!(chat.interrupts.is_empty());
}

#[tokio::test]
async fn typed_peer_and_mail_notices_wait_for_answer_completion() {
    let identity = codex_app_server_protocol::AgentInputIdentity {
        thread_id: ThreadId::new().to_string(),
        nickname: Some("Kant".to_string()),
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let mut recipient = identity.clone();
    recipient.thread_id = ThreadId::new().to_string();
    recipient.nickname = Some("Hilbert".to_string());
    let attribution = codex_app_server_protocol::AgentInputAttribution {
        sender: identity,
        recipient,
        sender_turn_id: "sender-turn".to_string(),
    };
    for (id, verb) in [
        (
            codex_protocol::protocol::new_attributed_agent_message_response_item_id().to_string(),
            "sends",
        ),
        (
            codex_protocol::mailbox_acceptance_receipt_id(&ThreadId::new().to_string())
                .expect("acceptance ID")
                .to_string(),
            "mails",
        ),
    ] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.thread_id = Some(ThreadId::new());
        let mut item = message(id, String::new(), MessagePhase::Commentary);
        let AppServerThreadItem::AgentMessage {
            attribution: item_attribution,
            input,
            ..
        } = &mut item
        else {
            unreachable!();
        };
        *item_attribution = Some(attribution.clone());
        *input = Some(vec![codex_app_server_protocol::UserInput::Text {
            text: "Peer payload.".to_string(),
            text_elements: Vec::new(),
        }]);
        chat.on_agent_message_delta("Answer.".to_string());
        deliver(&mut chat, item);
        assert!(output(&mut rx).is_empty());
        deliver(
            &mut chat,
            message(
                "authored".to_string(),
                "Answer.".to_string(),
                MessagePhase::FinalAnswer,
            ),
        );
        assert_eq!(
            output(&mut rx),
            vec![
                "answer: Answer.".to_string(),
                format!("• Kant {verb} to Hilbert:\n  └ Peer payload."),
            ]
        );
    }
}

#[tokio::test]
async fn own_collab_tool_lifecycle_is_not_queued_as_an_async_notice() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_agent_message_delta("Before the tool".to_string());
    deliver(
        &mut chat,
        completion(SubAgentCompletionModelVisibility::NotVisible),
    );
    let child = ThreadId::new();
    chat.on_collab_agent_tool_call(
        AppServerThreadItem::CollabAgentToolCall {
            id: "own-spawn".to_string(),
            tool: codex_app_server_protocol::CollabAgentTool::SpawnAgent,
            status: codex_app_server_protocol::CollabAgentToolCallStatus::Completed,
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            mailbox_input: None,
            sender_thread_id: ThreadId::new().to_string(),
            receiver_thread_ids: vec![child.to_string()],
            receiver_agents: Vec::new(),
            prompt: Some("Own tool input".to_string()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        },
        /*deadline_at_ms*/ None,
    );
    assert!(chat.stream_controller.is_none());
    assert!(!chat.interrupts.is_empty());
    let rendered = output(&mut rx);
    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0], "answer: Before the tool");
    assert!(rendered[1].contains("Spawned"));
}
