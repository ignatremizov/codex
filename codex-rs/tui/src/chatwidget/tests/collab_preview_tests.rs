use super::*;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells_with_preview_line_limits;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn activity_prompt_live_and_replay_use_local_preview_authority() {
    use codex_app_server_protocol::SubAgentActivityKind;

    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.agent_prompt_preview_lines = 2;
    chat.config.tui_agent_prompt_preview_lines = 99;
    for kind in [
        SubAgentActivityKind::Started,
        SubAgentActivityKind::Interacted,
    ] {
        let item = AppServerThreadItem::SubAgentActivity {
            id: format!("activity-{kind:?}"),
            kind,
            agent_thread_id: ThreadId::new().to_string(),
            agent_path: "/root/reviewer".into(),
            prompt: Some("first https://example.com\n  second\nlast".into()),
        };
        chat.handle_server_notification(
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                completed_at_ms: 0,
                item: item.clone(),
            }),
            /*replay_kind*/ None,
        );
        let mut live = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                live.push(cell);
            }
        }
        let replay = thread_items_to_transcript_cells_with_preview_line_limits(
            /*thread_id*/ None,
            &chat.config.cwd,
            [item],
            RawReasoningVisibility::Hidden,
            Some(&chat.config),
            OutputPreviewLineLimits::default(),
            (&chat.local_settings.tui).into(),
        );
        assert_eq!((live.len(), replay.len()), (1, 1));
        assert!(crate::terminal_hyperlinks::lines_with_sources_eq(
            &live[0].display_hyperlink_lines(/*width*/ 80),
            &replay[0].display_hyperlink_lines(/*width*/ 80),
        ));
        assert_eq!(live[0].raw_lines(), replay[0].raw_lines());
        assert_eq!(
            live[0]
                .display_lines(/*width*/ 80)
                .iter()
                .skip(1)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["  └ first https://example.com", "    … +2 rows hidden"],
        );
        assert_eq!(
            live[0]
                .raw_lines()
                .iter()
                .skip(1)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["  └ first https://example.com", "      second", "    last"],
        );
    }
}

#[tokio::test]
async fn collaboration_live_and_replay_use_local_limits_and_keep_full_raw_source() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.agent_prompt_preview_lines = 2;
    chat.local_settings.tui.agent_response_preview_lines = 2;
    chat.config.tui_agent_prompt_preview_lines = 99;
    chat.config.tui_agent_response_preview_lines = 99;
    let sender = ThreadId::new();
    let receiver = ThreadId::new();
    for tool in [
        AppServerCollabAgentTool::SpawnAgent,
        AppServerCollabAgentTool::Wait,
    ] {
        let item = AppServerThreadItem::CollabAgentToolCall {
            id: format!("call-{tool:?}"),
            tool,
            status: AppServerCollabAgentToolCallStatus::Completed,
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            sender_thread_id: sender.to_string(),
            receiver_thread_ids: vec![receiver.to_string()],
            receiver_agents: Vec::new(),
            prompt: Some("first\nsecond\nlast".into()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::from([(
                receiver.to_string(),
                AppServerCollabAgentState {
                    status: AppServerCollabAgentStatus::Completed,
                    message: Some("first\nsecond\nlast".into()),
                },
            )]),
        };
        chat.handle_server_notification(
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                completed_at_ms: 0,
                item: item.clone(),
            }),
            /*replay_kind*/ None,
        );
        let mut live = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                live.push(cell);
            }
        }
        assert_eq!(live.len(), 1);
        for command in [1, 100] {
            let replay = thread_items_to_transcript_cells_with_preview_line_limits(
                Some(sender),
                &chat.config.cwd,
                [item.clone()],
                RawReasoningVisibility::Hidden,
                Some(&chat.config),
                OutputPreviewLineLimits {
                    command,
                    user_shell: command,
                },
                (&chat.local_settings.tui).into(),
            );
            assert_eq!(replay.len(), 1);
            assert_eq!(
                live[0].display_hyperlink_lines(/*width*/ 80),
                replay[0].display_hyperlink_lines(/*width*/ 80)
            );
            assert!(crate::terminal_hyperlinks::lines_with_sources_eq(
                &live[0].display_hyperlink_lines(/*width*/ 80),
                &replay[0].display_hyperlink_lines(/*width*/ 80),
            ));
            assert_eq!(live[0].raw_lines(), replay[0].raw_lines());
        }
        let display = live[0]
            .display_lines(/*width*/ 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let raw = live[0]
            .raw_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(display.contains("… +2 rows hidden"));
        assert!(!display.contains("last"));
        assert!(raw.contains("last"));
        assert!(!raw.contains("rows hidden"));
    }
}
