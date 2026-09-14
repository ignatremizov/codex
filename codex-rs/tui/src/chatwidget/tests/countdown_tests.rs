use super::*;
use pretty_assertions::assert_eq;

fn poll(chat: &mut ChatWidget, item_id: &str, process_id: &str, deadline_at_ms: Option<i64>) {
    chat.handle_server_notification(
        ServerNotification::TerminalInteraction(
            codex_app_server_protocol::TerminalInteractionNotification {
                thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
                turn_id: "turn-1".into(),
                item_id: item_id.into(),
                process_id: process_id.into(),
                stdin: String::new(),
                deadline_at_ms,
                wait: None,
            },
        ),
        /*replay_kind*/ None,
    );
}

fn future_deadline() -> i64 {
    chrono::Utc::now().timestamp_millis() + 600_000
}

#[tokio::test]
async fn poll_clear_and_old_process_completion_cannot_clear_new_incarnation() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let mut old = begin_unified_exec_startup(&mut chat, "old-item", "proc", "sleep 60");
    poll(&mut chat, "old-item", "proc", Some(future_deadline()));
    begin_unified_exec_startup(&mut chat, "new-item", "proc", "sleep 120");
    poll(&mut chat, "new-item", "proc", Some(future_deadline()));
    let expected = chat.status_state.countdown_owner.clone();
    let header = chat.status_state.current_status.clone();
    poll(&mut chat, "old-item", "proc", /*deadline_at_ms*/ None);
    if let AppServerThreadItem::CommandExecution { status, .. } = &mut old {
        *status = AppServerCommandExecutionStatus::Completed;
    }
    handle_exec_end(&mut chat, old);
    assert_eq!(chat.status_state.countdown_owner, expected);
    assert_eq!(chat.status_state.current_status, header);
    assert_eq!(
        chat.unified_exec_processes
            .iter()
            .map(|process| process.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["new-item"]
    );
    poll(&mut chat, "new-item", "proc", /*deadline_at_ms*/ None);
    assert_eq!(chat.status_state.countdown_owner, None);
    assert!(!render_bottom_popup(&chat, /*width*/ 100).contains(" left"));
}

#[tokio::test]
async fn countdown_survives_hidden_row_but_not_status_replacement_or_compaction() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    begin_unified_exec_startup(&mut chat, "item", "proc", "sleep 60");
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    let owner = chat.status_state.countdown_owner.clone();
    chat.bottom_pane.hide_status_indicator();
    chat.bottom_pane.ensure_status_indicator();
    let status = chat.status_state.current_status.clone();
    chat.set_status(
        status.header,
        status.details,
        StatusDetailsCapitalization::Preserve,
        status.details_max_lines,
    );
    assert_eq!(chat.status_state.countdown_owner, owner);
    assert!(render_bottom_popup(&chat, /*width*/ 100).contains(" left"));
    chat.set_status_header("Thinking".into());
    assert_eq!(chat.status_state.countdown_owner, None);
    chat.status_state
        .pending_guardian_review_status
        .start_or_update("review".into(), "command".into());
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    assert_eq!(chat.status_state.countdown_owner, None);
    chat.status_state.pending_guardian_review_status.clear();
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    chat.on_context_compaction_started("compact".into(), "turn-1".into(), Duration::ZERO);
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    assert_eq!(chat.status_state.countdown_owner, None);
    assert!(!render_bottom_popup(&chat, /*width*/ 100).contains(" left"));
}

#[tokio::test]
async fn replay_and_stale_turn_notifications_never_arm_countdown() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let item = begin_unified_exec_startup(&mut chat, "item", "proc", "sleep 60");
    let start = ItemStartedNotification {
        thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
        turn_id: "turn-1".into(),
        item,
        started_at_ms: 0,
        deadline_at_ms: Some(future_deadline()),
    };
    for replay_kind in [
        ReplayKind::ThreadSnapshot,
        ReplayKind::ResumeInitialMessages,
    ] {
        chat.handle_server_notification(
            ServerNotification::ItemStarted(start.clone()),
            Some(replay_kind),
        );
        assert_eq!(chat.status_state.countdown_owner, None);
    }
    handle_turn_started(&mut chat, "turn-2");
    begin_unified_exec_startup(&mut chat, "current-item", "current-proc", "sleep 120");
    chat.on_terminal_interaction(codex_app_server_protocol::TerminalInteractionNotification {
        thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
        turn_id: "turn-2".into(),
        item_id: "current-item".into(),
        process_id: "current-proc".into(),
        stdin: String::new(),
        deadline_at_ms: Some(future_deadline()),
        wait: None,
    });
    let owner = chat.status_state.countdown_owner.clone();
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    chat.handle_server_notification(
        ServerNotification::ItemStarted(start),
        /*replay_kind*/ None,
    );
    assert_eq!(chat.status_state.countdown_owner, owner);
}

#[tokio::test]
async fn terminal_replay_preserves_stdin_history_without_mutating_live_countdown() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    begin_unified_exec_startup(&mut chat, "item", "proc", "sleep 60");
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    let owner = chat.status_state.countdown_owner.clone();
    drain_insert_history(&mut rx);
    for stdin in ["", "replayed stdin"] {
        chat.handle_server_notification(
            ServerNotification::TerminalInteraction(
                codex_app_server_protocol::TerminalInteractionNotification {
                    thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
                    turn_id: "turn-1".into(),
                    item_id: "item".into(),
                    process_id: "proc".into(),
                    stdin: stdin.into(),
                    deadline_at_ms: Some(future_deadline()),
                    wait: None,
                },
            ),
            Some(ReplayKind::ThreadSnapshot),
        );
    }
    assert_eq!(chat.status_state.countdown_owner, owner);
    let history = drain_insert_history_transcript(&mut rx);
    assert!(
        history
            .iter()
            .flatten()
            .any(|line| line.to_string().contains("replayed stdin"))
    );
}

#[tokio::test]
async fn live_start_without_estimate_replaces_countdown_but_unrelated_terminal_clear_does_not() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    begin_unified_exec_startup(&mut chat, "A", "proc-A", "sleep 60");
    begin_unified_exec_startup(&mut chat, "B", "proc-B", "sleep 120");
    poll(&mut chat, "A", "proc-A", Some(future_deadline()));
    let owner = chat.status_state.countdown_owner.clone();
    poll(&mut chat, "B", "proc-B", /*deadline_at_ms*/ None);
    assert_eq!(chat.status_state.countdown_owner, owner);
    begin_unified_exec_startup(&mut chat, "C", "proc-C", "sleep 180");
    assert_eq!(chat.status_state.countdown_owner, None);
    poll(&mut chat, "A", "proc-A", Some(future_deadline()));
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
            turn_id: "turn-1".into(),
            started_at_ms: 0,
            deadline_at_ms: None,
            item: AppServerThreadItem::CollabAgentToolCall {
                id: "wait".into(),
                tool: AppServerCollabAgentTool::Wait,
                status: AppServerCollabAgentToolCallStatus::InProgress,
                observe_commentary: None,
                wake_on_completion: None,
                target_messages: None,
                queue_input: None,
                sender_thread_id: ThreadId::new().to_string(),
                receiver_thread_ids: Vec::new(),
                receiver_agents: Vec::new(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::new(),
            },
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(chat.status_state.countdown_owner, None);
}

#[tokio::test]
async fn interruption_clears_countdown_even_while_mcp_keeps_row_running() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    chat.set_mcp_startup_expected_servers(["pending".to_string()]);
    chat.on_mcp_server_status_updated(McpServerStatusUpdatedNotification {
        thread_id: chat.thread_id.map(|id| id.to_string()),
        name: "pending".into(),
        status: McpServerStartupState::Starting,
        error: None,
        failure_reason: None,
    });
    begin_unified_exec_startup(&mut chat, "item", "proc", "sleep 60");
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    assert!(chat.status_state.countdown_owner.is_some());
    chat.finalize_turn();
    assert!(chat.bottom_pane.is_task_running());
    assert_eq!(chat.status_state.countdown_owner, None);
    poll(&mut chat, "item", "proc", Some(future_deadline()));
    assert_eq!(chat.status_state.countdown_owner, None);
    assert!(!render_bottom_popup(&chat, /*width*/ 100).contains(" left"));
}

#[tokio::test]
async fn finishing_old_agent_wait_preserves_new_wait_header_and_countdown() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    let wait = |id: &str, status| AppServerThreadItem::CollabAgentToolCall {
        id: id.into(),
        tool: AppServerCollabAgentTool::Wait,
        status,
        observe_commentary: None,
        wake_on_completion: None,
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
    chat.on_collab_agent_tool_call(
        wait("A", AppServerCollabAgentToolCallStatus::InProgress),
        Some(future_deadline()),
        "turn-1",
    );
    chat.on_collab_agent_tool_call(
        wait("B", AppServerCollabAgentToolCallStatus::InProgress),
        Some(future_deadline()),
        "turn-1",
    );
    let owner = chat.status_state.countdown_owner.clone();
    let status = chat.status_state.current_status.clone();
    chat.on_collab_agent_tool_call(
        wait("A", AppServerCollabAgentToolCallStatus::Completed),
        /*deadline_at_ms*/ None,
        "turn-1",
    );
    assert_eq!(chat.status_state.countdown_owner, owner);
    assert_eq!(chat.status_state.current_status, status);
    chat.on_collab_agent_tool_call(
        wait("B", AppServerCollabAgentToolCallStatus::Completed),
        /*deadline_at_ms*/ None,
        "turn-1",
    );
    assert_eq!(chat.status_state.countdown_owner, None);
}
