//! Explicit communication provenance keeps incoming history separate from local model output.

use super::*;
use codex_app_server_protocol::InterAgentMessageSource;
use pretty_assertions::assert_eq;

fn communication(id: &str, author: &str, text: &str) -> AppServerThreadItem {
    AppServerThreadItem::AgentMessage {
        id: id.into(),
        text: text.into(),
        attribution: None,
        input: None,
        phase: Some(MessagePhase::Commentary),
        memory_citation: None,
        delivery: None,
        questions: None,
        inter_agent_source: Some(InterAgentMessageSource {
            author: author.into(),
            recipient: "/root".into(),
        }),
    }
}

fn publish(chat: &mut ChatWidget, item: AppServerThreadItem) {
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: "thread-1".into(),
            turn_id: "communication-turn".into(),
            started_at_ms: 0,
            deadline_at_ms: None,
            item: item.clone(),
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".into(),
            turn_id: "communication-turn".into(),
            completed_at_ms: 0,
            item,
        }),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn attributed_scoped_message_does_not_complete_the_local_answer() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let author = ThreadId::new().to_string();
    let id = codex_protocol::protocol::new_attributed_agent_message_response_item_id().to_string();
    publish(
        &mut chat,
        communication(
            &id,
            &author,
            &format!("Agent message from `{author}`:\n\nReview this."),
        ),
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    let rendered = lines_to_single_string(&cells[0]);
    assert!(rendered.contains("sends:"), "{rendered}");
    assert!(rendered.contains("Review this."), "{rendered}");
    assert_eq!(&chat.transcript.last_completed_agent_message, &None);
    assert_eq!(&chat.transcript.last_agent_markdown, &None);
}

#[tokio::test]
async fn live_app_server_inter_agent_message_renders_in_transcript() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    publish(
        &mut chat,
        communication(
            "amsg_message",
            "/root",
            "Agent message from `/root`:\n\nInspect the repository.",
        ),
    );
    let cells = drain_insert_history_cells(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_eq!(
        cells[0].transcript_navigation_kind(),
        Some(crate::history_cell::TranscriptNavigationKind::Commentary)
    );
    let rendered =
        lines_to_single_string(&cells[0].display_lines(/*width*/ 80)).replace("  \n", "\n");
    insta::assert_snapshot!(
        "live_app_server_inter_agent_message_renders_in_transcript",
        rendered
    );
    assert_eq!(&chat.transcript.last_completed_agent_message, &None);
    assert_eq!(&chat.transcript.last_agent_markdown, &None);
}

#[tokio::test]
async fn live_app_server_inter_agent_final_answer_renders_in_transcript() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    publish(
        &mut chat,
        communication(
            "amsg_final",
            "/root/direct_input_demo",
            "Agent final answer from `/root/direct_input_demo`:\n\nLorem ipsum dolor sit amet.",
        ),
    );
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    let rendered = lines_to_single_string(&cells[0]).replace("  \n", "\n");
    insta::assert_snapshot!(
        "live_app_server_inter_agent_final_answer_renders_in_transcript",
        rendered
    );
}

#[tokio::test]
async fn communication_preserves_active_answer_and_does_not_open_questions() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    handle_agent_message_delta(&mut chat, "Authoritative ");
    chat.run_commit_tick();
    let text = "Agent message from `/root/worker`:\n\nIncoming **task**.";
    let mut item = communication("amsg_incoming", "/root/worker", text);
    if let AppServerThreadItem::AgentMessage {
        questions, phase, ..
    } = &mut item
    {
        *phase = Some(MessagePhase::FinalAnswer);
        *questions = Some(vec![codex_protocol::items::AsyncUserInputQuestion {
            title: "Not a local assistant question".into(),
            options: None,
        }]);
    }
    publish(&mut chat, item);
    assert!(chat.stream_controller.is_some());
    assert!(!chat.interrupts.is_empty());
    assert_eq!(&chat.transcript.last_completed_agent_message, &None);
    assert!(chat.realtime_conversation.agent_items.is_empty());
    assert_eq!(
        chat.bottom_pane.questions.as_deref().map_or(
            /*default*/ 0,
            crate::bottom_pane::AsyncQuestions::unanswered_count
        ),
        0,
    );
    handle_agent_message_delta(&mut chat, "answer.");
    complete_assistant_message(
        &mut chat,
        "model-answer",
        "Authoritative answer.",
        Some(MessagePhase::FinalAnswer),
    );
    let mut finalized = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AppEvent::ConsolidateAgentMessage { source, .. } => finalized.push(("answer", source)),
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<history_cell::AgentMarkdownCell>() =>
            {
                finalized.push((
                    "communication",
                    cell.raw_lines()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            }
            _ => {}
        }
    }
    assert_eq!(
        finalized,
        vec![
            ("answer", "Authoritative answer.".into()),
            ("communication", text.into()),
        ]
    );
    assert_eq!(
        chat.transcript.last_completed_agent_message,
        Some(("turn-1".into(), "model-answer".into()))
    );
    assert_eq!(
        chat.transcript.last_agent_markdown.as_deref(),
        Some("Authoritative answer.")
    );
    assert!(chat.interrupts.is_empty());
}

#[tokio::test]
async fn canonical_and_legacy_communication_replay_keep_full_source() {
    let text = "Agent message from `/root/worker`:\n\nhttps://example.com/path\n\n::git-create-branch{cwd=\"/ignored\" branch=\"ignored\"}\n\nlast";
    for id in ["amsg_canonical", "item-2"] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.local_settings.tui.agent_prompt_preview_lines = 1;
        chat.local_settings.tui.agent_response_preview_lines = 1;
        let item = communication(id, "/root/worker", text);
        if id == "amsg_canonical" {
            publish(&mut chat, item.clone());
        } else {
            chat.replay_thread_item(
                item.clone(),
                "history-turn".into(),
                ReplayKind::ResumeInitialMessages,
            );
        }
        let replay = crate::thread_transcript::thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &chat.config.cwd,
            [item],
            crate::thread_transcript::RawReasoningVisibility::Hidden,
            Some(&chat.config),
        );
        let mut rendered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                rendered.push(cell);
            }
        }
        assert_eq!((rendered.len(), replay.len()), (1, 1));
        assert_eq!(rendered[0].raw_lines(), replay[0].raw_lines());
        assert_eq!(
            rendered[0]
                .raw_lines()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            text
        );
        for width in [20, 80] {
            let live_lines = rendered[0].display_hyperlink_lines(width);
            assert!(crate::terminal_hyperlinks::lines_with_sources_eq(
                &live_lines,
                &replay[0].display_hyperlink_lines(width),
            ));
            assert!(live_lines.iter().any(|line| !line.hyperlinks.is_empty()));
        }
        assert_eq!(&chat.transcript.last_agent_markdown, &None);
    }
}

#[tokio::test]
async fn ordinary_message_with_communication_looking_text_still_completes_the_answer() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let text = "Agent message from `/root/worker`:\n\nThis is ordinary assistant text.";
    let mut item = communication("amsg_lookalike", "/root/worker", text);
    if let AppServerThreadItem::AgentMessage {
        inter_agent_source, ..
    } = &mut item
    {
        *inter_agent_source = None;
    }
    publish(&mut chat, item);
    assert_eq!(
        chat.transcript.last_completed_agent_message,
        Some(("communication-turn".into(), "amsg_lookalike".into())),
    );
    assert_eq!(chat.transcript.last_agent_markdown.as_deref(), Some(text));
}

#[tokio::test]
async fn opaque_communication_uses_the_fixed_projected_placeholder() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    publish(
        &mut chat,
        communication(
            "amsg_opaque",
            "/root/worker",
            "Agent message from `/root/worker`:\n\nInput message encrypted",
        ),
    );
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    insta::assert_snapshot!(lines_to_single_string(&cells[0]).replace("  \n", "\n"), @r"
    • Agent message from /root/worker:

      Input message encrypted
    ");
}
