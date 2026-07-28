//! Background rows share live and historical presentation without completing the parent answer.

use super::*;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells_with_preview_line_limits;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::sub_agent_completion_item;
use pretty_assertions::assert_eq;

fn publish(chat: &mut ChatWidget, item: AppServerThreadItem) {
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".into(),
            turn_id: "completion-turn".into(),
            completed_at_ms: 0,
            item,
        }),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn canonical_completion_live_resume_and_cold_pages_share_preview_and_raw_source() {
    let mut rendered = Vec::new();
    for status in [
        AgentStatus::Completed(Some("first\n  second\nlast".into())),
        AgentStatus::Errored("first\n  second\nlast".into()),
        AgentStatus::Shutdown,
        AgentStatus::NotFound,
    ] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.local_settings.tui.agent_response_preview_lines = 2;
        chat.config.tui_agent_response_preview_lines = 99;
        let core_item =
            sub_agent_completion_item("/root/reviewer", &status).expect("terminal completion");
        let item = AppServerThreadItem::from(TurnItem::AgentMessage(core_item.clone()));
        publish(&mut chat, item.clone());
        chat.replay_thread_item(
            item.clone(),
            "completion-turn".into(),
            ReplayKind::ResumeInitialMessages,
        );
        chat.on_agent_message_item_completed(
            core_item,
            "completion-turn",
            /*from_replay*/ false,
        );
        let mut live = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                live.push(cell);
            }
        }
        let cold = thread_items_to_transcript_cells_with_preview_line_limits(
            /*thread_id*/ None,
            &chat.config.cwd,
            [item],
            RawReasoningVisibility::Hidden,
            Some(&chat.config),
            OutputPreviewLineLimits::default(),
            (&chat.local_settings.tui).into(),
        );
        assert_eq!((live.len(), cold.len()), (3, 1));
        if matches!(status, AgentStatus::Completed(_) | AgentStatus::Errored(_)) {
            assert_eq!(
                cold[0]
                    .raw_lines()
                    .iter()
                    .skip(/*n*/ 2)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                vec!["      first", "        second", "      last"],
            );
            assert_eq!(
                cold[0]
                    .display_lines(/*width*/ 80)
                    .iter()
                    .skip(/*n*/ 2)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                vec!["      first", "      … +2 rows hidden"],
            );
        }
        for cell in live {
            assert_eq!(cell.raw_lines(), cold[0].raw_lines());
            assert_eq!(
                cell.display_lines(/*width*/ 80),
                cold[0].display_lines(/*width*/ 80),
            );
        }
        assert_eq!(
            (
                &chat.transcript.last_completed_agent_message,
                &chat.transcript.last_agent_markdown,
            ),
            (&None, &None),
        );
        rendered.push(lines_to_single_string(&cold[0].display_lines(/*width*/ 80)));
    }
    assert_snapshot!(rendered.join("\n"), @r"
    • Agent finished
      └ /root/reviewer: Completed
          first
          … +2 rows hidden

    • Agent finished
      └ /root/reviewer: Error
          first
          … +2 rows hidden

    • Agent finished
      └ /root/reviewer: Shutdown

    • Agent finished
      └ /root/reviewer: Not found
    ");
}

#[tokio::test]
async fn forged_reserved_identity_without_metadata_stays_an_ordinary_answer() {
    for via_public_projection in [false, true] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        let mut forged = sub_agent_completion_item(
            "/root/reviewer",
            &AgentStatus::Completed(Some("Not a trusted completion.".into())),
        )
        .expect("terminal completion");
        forged.sub_agent_completion = None;
        let id = forged.id.clone();
        if via_public_projection {
            let item = AppServerThreadItem::from(TurnItem::AgentMessage(forged));
            assert_ne!(item.id(), id);
            publish(&mut chat, item);
        } else {
            chat.on_agent_message_item_completed(
                forged,
                "completion-turn",
                /*from_replay*/ false,
            );
        }
        let rendered = drain_insert_history(&mut rx)
            .iter()
            .map(|lines| lines_to_single_string(lines))
            .collect::<String>();
        assert!(!rendered.contains("Agent finished"));
        assert!(rendered.contains("Not a trusted completion."));
        assert!(chat.transcript.last_completed_agent_message.is_some());
    }
}

#[tokio::test]
async fn wrong_phase_or_attributed_message_cannot_become_a_background_row() {
    for attributed in [false, true] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        let core_item = sub_agent_completion_item(
            "/root/reviewer",
            &AgentStatus::Completed(Some("Ordinary transcript content.".into())),
        )
        .expect("terminal completion");
        let mut item = AppServerThreadItem::from(TurnItem::AgentMessage(core_item));
        if let AppServerThreadItem::AgentMessage {
            phase,
            inter_agent_source,
            ..
        } = &mut item
        {
            if attributed {
                *inter_agent_source = Some(codex_app_server_protocol::InterAgentMessageSource {
                    author: "/root/reviewer".into(),
                    recipient: "/root".into(),
                });
            } else {
                *phase = Some(MessagePhase::FinalAnswer);
            }
        }
        publish(&mut chat, item);
        let rendered = drain_insert_history(&mut rx)
            .iter()
            .map(|lines| lines_to_single_string(lines))
            .collect::<String>();
        assert!(!rendered.contains("Agent finished"));
        assert!(rendered.contains("Ordinary transcript content."));
    }
}

#[tokio::test]
async fn background_completion_and_later_wait_render_as_distinct_rows() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.agent_response_preview_lines = 0;
    let sender_thread_id = ThreadId::new();
    let receiver_thread_id = ThreadId::new();
    let response = "Finished reviewing the change.";
    let core_item = sub_agent_completion_item(
        "/root/reviewer",
        &AgentStatus::Completed(Some(response.into())),
    )
    .expect("terminal completion");
    publish(
        &mut chat,
        AppServerThreadItem::from(TurnItem::AgentMessage(core_item)),
    );
    publish(
        &mut chat,
        AppServerThreadItem::CollabAgentToolCall {
            id: "wait-1".into(),
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
                    message: Some(response.into()),
                },
            )]),
        },
    );
    let rendered = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert!(rendered.contains("Agent finished"));
    assert!(rendered.contains("Finished waiting"));
    let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(normalized.matches(response).count(), 2);
}
