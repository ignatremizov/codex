use super::*;
use codex_app_server_protocol::MailboxReadItem;
use codex_app_server_protocol::MailboxReadSelector;
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

fn start_item(chat: &mut ChatWidget, item: AppServerThreadItem) {
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: chat
                .thread_id
                .map_or_else(|| "thread-1".to_string(), |id| id.to_string()),
            turn_id: "turn-1".to_string(),
            started_at_ms: 0,
            deadline_at_ms: None,
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
async fn live_mailbox_read_waits_for_answer_completion() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_agent_message_delta("Answer before".to_string());
    deliver(
        &mut chat,
        AppServerThreadItem::MailboxRead(MailboxReadItem {
            id: "check-mail-live".to_string(),
            selector: MailboxReadSelector::User,
            consumed_count: 1,
            rejected_count: 0,
        }),
    );

    assert!(chat.stream_controller.is_some());
    assert!(output(&mut rx).is_empty());

    chat.on_agent_message_delta(" and after.".to_string());
    deliver(
        &mut chat,
        message(
            "main-final".to_string(),
            "Answer before and after.".to_string(),
            MessagePhase::FinalAnswer,
        ),
    );

    assert_eq!(
        output(&mut rx),
        vec![
            "answer: Answer before and after.".to_string(),
            "• Checked mailbox from user · 1 message consumed".to_string(),
        ],
    );
}

#[tokio::test]
async fn replayed_mailbox_read_remains_immediate() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_agent_message_delta("Live answer before replay.".to_string());

    chat.replay_thread_item(
        AppServerThreadItem::MailboxRead(MailboxReadItem {
            id: "check-mail-replay".to_string(),
            selector: MailboxReadSelector::User,
            consumed_count: 1,
            rejected_count: 0,
        }),
        "replayed-turn".to_string(),
        ReplayKind::ResumeInitialMessages,
    );

    assert_eq!(
        output(&mut rx),
        vec![
            "answer: Live answer before replay.".to_string(),
            "• Checked mailbox from user · 1 message consumed".to_string(),
        ],
    );
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
        batch_id: None,
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
async fn child_batch_live_and_replay_use_one_grouped_root_presentation() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    let sender = ThreadId::new();
    let accepted = ThreadId::new();
    let rejected = ThreadId::new();
    chat.set_collab_agent_metadata(
        sender,
        Some("Child".to_string()),
        Some("worker".to_string()),
    );
    chat.set_collab_agent_task_path(sender, Some("/root/child".to_string()));
    chat.set_collab_agent_ref(sender, "7".to_string());
    chat.set_collab_agent_metadata(
        accepted,
        Some("Reviewer".to_string()),
        Some("coder".to_string()),
    );
    chat.set_collab_agent_ref(accepted, "8".to_string());
    chat.set_collab_agent_metadata(
        rejected,
        Some("Tester".to_string()),
        Some("tester".to_string()),
    );
    chat.set_collab_agent_ref(rejected, "9".to_string());

    let batch = AppServerThreadItem::CollabAgentToolCall {
        id: "child-send-input-batch".to_string(),
        tool: codex_app_server_protocol::CollabAgentTool::SendInput,
        status: codex_app_server_protocol::CollabAgentToolCallStatus::Failed,
        observe_commentary: Some(true),
        wake_on_completion: Some(false),
        target_messages: Some(true),
        queue_input: Some(true),
        mailbox_input: None,
        input_batch: Some(codex_protocol::CollabAgentInputBatch {
            flags: "cmq".to_string(),
            sender_thread_id: Some(sender),
            results: vec![
                codex_protocol::CollabAgentInputResult {
                    target: "8".to_string(),
                    receiver_thread_id: Some(accepted.to_string()),
                    status: codex_protocol::CollabAgentInputStatus::Submitted,
                    error: None,
                    hint: None,
                },
                codex_protocol::CollabAgentInputResult {
                    target: "9".to_string(),
                    receiver_thread_id: Some(rejected.to_string()),
                    status: codex_protocol::CollabAgentInputStatus::Error,
                    error: Some("receiver rejected the input".to_string()),
                    hint: None,
                },
            ],
        }),
        sender_thread_id: sender.to_string(),
        receiver_thread_ids: vec![accepted.to_string(), rejected.to_string()],
        receiver_agents: vec![
            codex_app_server_protocol::CollabAgentRef {
                thread_id: accepted.to_string(),
                agent_ref: Some("8".to_string()),
                agent_nickname: Some("Reviewer".to_string()),
                agent_role: Some("coder".to_string()),
                task_path: None,
            },
            codex_app_server_protocol::CollabAgentRef {
                thread_id: rejected.to_string(),
                agent_ref: Some("9".to_string()),
                agent_nickname: Some("Tester".to_string()),
                agent_role: Some("tester".to_string()),
                task_path: None,
            },
        ],
        prompt: Some("Shared child instruction.".to_string()),
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::new(),
    };

    chat.on_agent_message_delta("Main keeps ".to_string());
    deliver(&mut chat, batch.clone());
    assert!(chat.stream_controller.is_some());
    assert_eq!(output(&mut rx), Vec::<String>::new());
    chat.on_agent_message_delta("its answer intact.".to_string());
    deliver(
        &mut chat,
        message(
            "main-final".to_string(),
            "Main keeps its answer intact.".to_string(),
            MessagePhase::FinalAnswer,
        ),
    );
    let mut live = output(&mut rx);
    assert_eq!(live.remove(0), "answer: Main keeps its answer intact.");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].matches("Shared child instruction.").count(), 1);
    assert!(op_rx.try_recv().is_err());

    chat.replay_thread_item(
        batch,
        "child-turn".to_string(),
        ReplayKind::ResumeInitialMessages,
    );
    let replay = output(&mut rx);
    assert_eq!(replay, live);
    insta::assert_snapshot!(live[0], @r"
    • Input batch from Child [worker] /root/child (7) completed with errors
      └ Sent input to Reviewer [coder] (8) (receive commentary · no wake on completion · allow replies · queue turn + reply)
        Send error for Tester [tester] (9) (receive commentary · no wake on completion · allow replies · queue turn + reply)
        receiver rejected the input
        Shared child instruction.
    ");
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
            input_batch: None,
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
        "turn-1",
        ThreadItemRenderSource::Live,
    );
    assert!(chat.stream_controller.is_none());
    assert!(chat.interrupts.is_empty());
    let rendered = output(&mut rx);
    assert_eq!(rendered.len(), 3);
    assert_eq!(rendered[0], "answer: Before the tool");
    assert!(rendered[1].contains("completed"));
    assert!(rendered[2].contains("Spawned"));
}

#[tokio::test]
async fn commentary_streaming_does_not_defer_async_notices() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    start_item(
        &mut chat,
        AppServerThreadItem::AgentMessage {
            id: "commentary-item".to_string(),
            text: String::new(),
            attribution: None,
            input: None,
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
        },
    );
    chat.on_agent_message_delta("Interim commentary".to_string());
    assert!(chat.stream_controller.is_some());
    assert!(!chat.is_streaming_final_answer());

    deliver(
        &mut chat,
        completion(SubAgentCompletionModelVisibility::NotVisible),
    );
    assert!(chat.interrupts.is_empty());
    let rendered = output(&mut rx);
    assert_eq!(
        rendered,
        vec![
            "answer: Interim commentary".to_string(),
            "• /root/reviewer completed: (○ not visible)\n  └ Finished review.".to_string(),
        ]
    );
}

#[tokio::test]
async fn plan_streaming_defers_async_notices_until_plan_completed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5")).await;
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let plan_mask = collaboration_modes::mask_for_kind(chat.model_catalog.as_ref(), ModeKind::Plan)
        .expect("expected plan collaboration mask");
    chat.set_collaboration_mask(plan_mask);
    chat.on_task_started();
    chat.on_plan_delta("- Step 1\n".to_string());
    assert!(chat.is_streaming_final_answer());

    deliver(
        &mut chat,
        completion(SubAgentCompletionModelVisibility::NotVisible),
    );
    assert!(!chat.interrupts.is_empty());
    assert!(output(&mut rx).is_empty());

    chat.on_plan_item_completed("- Step 1\n".to_string());
    assert!(chat.interrupts.is_empty());
    let rendered = output(&mut rx);
    assert_eq!(
        rendered,
        vec!["• /root/reviewer completed: (○ not visible)\n  └ Finished review.".to_string()]
    );
}
