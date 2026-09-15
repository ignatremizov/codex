//! Background rows share live and historical presentation without completing the parent answer.

use super::*;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells_with_preview_line_limits;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::sub_agent_completion_item;
use codex_protocol::protocol::sub_agent_completion_item_with_visibility;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn partial_replayed_spawn_metadata_enriches_labels_without_erasing_navigation_identity() {
    let (mut chat, _events, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    let request = crate::multi_agents::SpawnRequestSummary {
        model: Some("recorded-model".into()),
        reasoning_effort: None,
    };
    chat.set_collab_agent_metadata(
        thread_id,
        Some("Current name".into()),
        Some("reviewer".into()),
    );
    chat.set_collab_agent_ref(thread_id, "7".into());
    chat.merge_collab_agent_metadata(
        thread_id,
        AgentMetadata {
            spawn_request: Some(request.clone()),
            ..Default::default()
        },
    );
    assert_eq!(
        chat.collab_agent_metadata(thread_id),
        AgentMetadata {
            agent_ref: Some("7".into()),
            task_path: crate::multi_agents::AgentTaskPath::Unknown,
            agent_nickname: Some("Current name".into()),
            agent_role: Some("reviewer".into()),
            spawn_request: Some(request.clone()),
        }
    );
    chat.set_collab_agent_metadata(thread_id, Some("Renamed".into()), Some("reviewer".into()));
    chat.merge_collab_agent_metadata(thread_id, AgentMetadata::default());
    assert_eq!(
        chat.collab_agent_metadata(thread_id),
        AgentMetadata {
            agent_ref: Some("7".into()),
            task_path: crate::multi_agents::AgentTaskPath::Unknown,
            agent_nickname: Some("Renamed".into()),
            agent_role: Some("reviewer".into()),
            spawn_request: Some(request),
        }
    );
}

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
async fn confirmed_task_path_clear_survives_legacy_updates_in_live_and_cold_labels() {
    let target = ThreadId::new();
    let source = ThreadId::new();
    let established: AppServerThreadItem = serde_json::from_value(serde_json::json!({
        "type": "collabAgentToolCall", "id": "named-task", "tool": "spawnAgent",
        "status": "completed", "senderThreadId": source.to_string(),
        "receiverThreadIds": [target.to_string()],
        "receiverAgents": [{"threadId": target.to_string(), "agentNickname": "Worker", "agentRef": "2",
            "taskPath": "/root/confirmed"}], "agentsStates": {}
    })).expect("recorded task identity");
    let legacy: AppServerThreadItem = serde_json::from_value(serde_json::json!({
        "type": "collabAgentToolCall", "id": "legacy-wait", "tool": "wait",
        "status": "completed", "senderThreadId": source.to_string(),
        "receiverThreadIds": [target.to_string()],
        "receiverAgents": [{"threadId": target.to_string()}], "agentsStates": {}
    }))
    .expect("legacy item without path authority");
    for status in ["failed", "unknown", "succeeded"] {
        let mapping: AppServerThreadItem = serde_json::from_value(serde_json::json!({
            "type": "userAgentControl", "id": "path-clear", "action": "resume",
            "targetThreadId": target.to_string(), "status": status, "resumedTarget": false,
            "ref": "3",
            "taskPathMapping": [{"threadId": target.to_string(), "taskPath": null}]
        }))
        .expect("path mapping audit");
        let items = [established.clone(), mapping, legacy.clone()];
        let cold = crate::thread_transcript::collab_agent_metadata_from_items(items.iter());
        let expected_path = crate::multi_agents::AgentTaskPath::Known(
            (status != "succeeded").then(|| "/root/confirmed".to_string()),
        );
        assert_eq!(cold[&target].task_path, expected_path);
        assert_eq!(
            cold[&target].agent_ref.as_deref(),
            Some(if status == "succeeded" { "3" } else { "2" })
        );
        for replay in [false, true] {
            let (mut chat, _events, _operations) =
                make_chatwidget_manual(/*model_override*/ None).await;
            for item in items.clone() {
                if replay {
                    chat.replay_thread_item(
                        item,
                        "audit-turn".to_string(),
                        ReplayKind::ResumeInitialMessages,
                    );
                } else {
                    publish(&mut chat, item);
                }
            }
            assert_eq!(chat.collab_agent_metadata(target), cold[&target]);
            chat.merge_collab_agent_metadata(target, AgentMetadata::default());
            assert_eq!(chat.collab_agent_metadata(target), cold[&target]);
        }
    }
}

#[tokio::test]
async fn failed_or_unknown_control_keeps_live_replayed_and_cold_agent_labels_consistent() {
    let thread_id = ThreadId::new();
    let confirmed: AppServerThreadItem = serde_json::from_value(serde_json::json!({
        "type": "userAgentControl", "id": "confirmed-spawn", "action": "spawn",
        "targetThreadId": thread_id.to_string(), "nickname": "Confirmed name",
        "role": "reviewer", "model": "confirmed-model", "reasoningEffort": "high",
        "resumedTarget": false, "status": "succeeded",
    }))
    .expect("confirmed spawn metadata");
    for status in ["failed", "unknown"] {
        let unconfirmed: AppServerThreadItem = serde_json::from_value(serde_json::json!({
            "type": "userAgentControl", "id": "unconfirmed-control", "action": "spawn",
            "targetThreadId": thread_id.to_string(), "nickname": "Requested name",
            "role": "worker", "model": "requested-model", "reasoningEffort": "low",
            "resumedTarget": false, "status": status,
        }))
        .expect("unconfirmed control audit");
        let expected = crate::thread_transcript::collab_agent_metadata_from_items([&confirmed]);
        assert_eq!(
            crate::thread_transcript::collab_agent_metadata_from_items([&confirmed, &unconfirmed]),
            expected,
        );
        for replay in [false, true] {
            let (mut chat, _events, _operations) =
                make_chatwidget_manual(/*model_override*/ None).await;
            for item in [confirmed.clone(), unconfirmed.clone()] {
                if replay {
                    chat.replay_thread_item(
                        item,
                        "control-audit".into(),
                        ReplayKind::ResumeInitialMessages,
                    );
                } else {
                    publish(&mut chat, item);
                }
            }
            assert_eq!(chat.collab_agent_metadata(thread_id), expected[&thread_id]);
        }
    }
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
                    .skip(/*n*/ 1)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                vec!["  └ first", "      second", "    last"],
            );
            assert_eq!(
                cold[0]
                    .display_lines(/*width*/ 80)
                    .iter()
                    .skip(/*n*/ 1)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                vec!["  └ first", "    … +2 rows hidden"],
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
        assert_eq!(chat.transcript.copyable_agent_message(), None);
        rendered.push(lines_to_single_string(&cold[0].display_lines(/*width*/ 80)));
    }
    assert_snapshot!(rendered.join("\n"), @r"
    • /root/reviewer completed: (● visible)
      └ first
        … +2 rows hidden

    • /root/reviewer errored (● visible):
      └ first
        … +2 rows hidden

    • /root/reviewer shut down (● visible)

    • /root/reviewer not found (● visible)
    ");
}

#[tokio::test]
async fn background_completion_shows_model_visibility_without_changing_parent_answer() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.set_primary_collab_agent_metadata(thread_id);
    for visibility in [
        SubAgentCompletionModelVisibility::Visible,
        SubAgentCompletionModelVisibility::NotVisible,
    ] {
        let item = sub_agent_completion_item_with_visibility(
            &thread_id.to_string(),
            &AgentStatus::Completed(Some("Finished.".into())),
            visibility,
        )
        .expect("terminal completion");
        publish(
            &mut chat,
            AppServerThreadItem::from(TurnItem::AgentMessage(item)),
        );
    }
    let cells = drain_insert_history(&mut rx);
    let markers = cells
        .iter()
        .flatten()
        .flat_map(|line| &line.spans)
        .filter(|span| matches!(span.content.as_ref(), "● visible" | "○ not visible"))
        .map(|span| (span.content.to_string(), span.style.fg))
        .collect::<Vec<_>>();
    assert_eq!(
        markers,
        vec![
            ("● visible".to_string(), Some(ratatui::style::Color::Green)),
            (
                "○ not visible".to_string(),
                Some(ratatui::style::Color::Cyan)
            ),
        ],
    );
    assert_snapshot!(
        cells.iter().map(|lines| lines_to_single_string(lines)).collect::<Vec<_>>().join("\n"),
        @r"
    • Main [default] completed: (● visible)
      └ Finished.


    • Main [default] completed: (○ not visible)
      └ Finished.
    "
    );
    assert_eq!(
        (
            &chat.transcript.last_completed_agent_message,
            &chat.transcript.last_agent_markdown
        ),
        (&None, &None),
    );
}

#[tokio::test]
async fn root_background_completion_uses_main_label() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let item = sub_agent_completion_item(
        "/root",
        &AgentStatus::Completed(Some("Parent task finished.".into())),
    )
    .expect("terminal completion");
    publish(
        &mut chat,
        AppServerThreadItem::from(TurnItem::AgentMessage(item)),
    );
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_snapshot!(lines_to_single_string(&cells[0]), @r"
    • Main [default] completed: (● visible)
      └ Parent task finished.
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
        assert!(!rendered.contains("completed (● visible)"));
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
        assert!(!rendered.contains("completed (● visible)"));
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
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            input_batch: None,
            mailbox_input: None,
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
    assert!(rendered.contains("/root/reviewer completed: (● visible)"));
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
    let completion = sub_agent_completion_item(
        &receiver_thread_id.to_string(),
        &AgentStatus::Completed(Some("Cinnamon".into())),
    )
    .expect("terminal completion");
    publish(
        &mut chat,
        AppServerThreadItem::from(TurnItem::AgentMessage(completion)),
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_snapshot!(
        lines_to_single_string(&cells[0]),
        @r"
    • Herschel [default] completed: (● visible)
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
    let completion = sub_agent_completion_item(
        &receiver_thread_id.to_string(),
        &AgentStatus::Completed(Some("Cinnamon".into())),
    )
    .expect("terminal completion");

    chat.replay_thread_item(
        AppServerThreadItem::CollabAgentToolCall {
            id: "spawn-1".to_string(),
            tool: AppServerCollabAgentTool::SpawnAgent,
            status: AppServerCollabAgentToolCallStatus::Completed,
            observe_commentary: Some(false),
            wake_on_completion: Some(false),
            target_messages: Some(false),
            queue_input: Some(false),
            input_batch: None,
            mailbox_input: None,
            sender_thread_id: sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_thread_id.to_string()],
            receiver_agents: vec![codex_app_server_protocol::CollabAgentRef {
                thread_id: receiver_thread_id.to_string(),
                agent_ref: Some("5".to_string()),
                task_path: None,
                agent_nickname: Some("Herschel".to_string()),
                agent_role: Some("default".to_string()),
            }],
            prompt: Some("Review the metadata presentation change.".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            reasoning_effort: Some(ReasoningEffortConfig::High),
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
            target_messages: Some(false),
            queue_input: Some(false),
            input_batch: None,
            mailbox_input: None,
            sender_thread_id: sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_thread_id.to_string()],
            receiver_agents: vec![codex_app_server_protocol::CollabAgentRef {
                thread_id: receiver_thread_id.to_string(),
                agent_ref: None,
                task_path: None,
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
        AppServerThreadItem::from(TurnItem::AgentMessage(completion)),
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
    • Spawned Herschel [default] (5) (gpt-5.6-sol high) (no commentary · no wake on completion)
      └ Review the metadata presentation change.


    • Sent input to Herschel [default] (5) (gpt-5.6-sol high) (no commentary · no wake on completion)
      └ Give me one random ingredient.


    • Herschel [default] (5) (gpt-5.6-sol high) completed: (● visible)
      └ Cinnamon
    "
    );
}
