use super::*;
use pretty_assertions::assert_eq;

fn completed_message(turn_id: &str, item_id: &str) -> ItemCompletedNotification {
    ItemCompletedNotification {
        thread_id: "thread-1".to_string(),
        turn_id: turn_id.to_string(),
        completed_at_ms: 0,
        item: AppServerThreadItem::AgentMessage {
            id: item_id.to_string(),
            text: "Received marker.".to_string(),
            phase: Some(MessagePhase::FinalAnswer),
            memory_citation: None,
            attribution: None,
            input: None,
            delivery: None,
            questions: None,
        },
    }
}

fn completed_turn(message: &ItemCompletedNotification) -> TurnCompletedNotification {
    let mut turn = app_server_turn(
        &message.turn_id,
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    turn.items_view = codex_app_server_protocol::TurnItemsView::Summary;
    turn.items = vec![message.item.clone()];
    TurnCompletedNotification {
        thread_id: message.thread_id.clone(),
        turn,
    }
}

#[tokio::test]
async fn live_repeated_completion_does_not_start_a_second_answer_stream() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    while rx.try_recv().is_ok() {}
    handle_agent_message_delta(&mut chat, "Received marker.");
    let message = completed_message("turn-1", "msg-1");
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message.clone()),
        /*replay_kind*/ None,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_snapshot!(lines_to_single_string(&cells[0]), @r"
    • Received marker.
    ");

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message.clone()),
        /*replay_kind*/ None,
    );
    assert!(
        rx.try_recv().is_err(),
        "duplicate must not enqueue rendering work"
    );
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(completed_turn(&message)),
        /*replay_kind*/ None,
    );
    while rx.try_recv().is_ok() {}
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message),
        /*replay_kind*/ None,
    );
    assert!(
        rx.try_recv().is_err(),
        "late duplicate must not restart the stream"
    );
}

#[tokio::test]
async fn turn_completion_fallback_owns_a_late_item_completion() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    while rx.try_recv().is_ok() {}
    let message = completed_message("turn-1", "msg-1");
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(completed_turn(&message)),
        /*replay_kind*/ None,
    );
    assert_eq!(drain_insert_history(&mut rx).len(), 1);
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message),
        /*replay_kind*/ None,
    );
    assert!(
        rx.try_recv().is_err(),
        "fallback already rendered this identity"
    );
}

#[tokio::test]
async fn equal_text_with_distinct_item_or_turn_identity_is_preserved() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut rendered = Vec::new();
    for (turn_id, item_id, phase) in [
        ("turn-1", "commentary", MessagePhase::Commentary),
        ("turn-1", "final", MessagePhase::FinalAnswer),
        ("turn-2", "final", MessagePhase::FinalAnswer),
    ] {
        let mut message = completed_message(turn_id, item_id);
        let AppServerThreadItem::AgentMessage { phase: value, .. } = &mut message.item else {
            unreachable!();
        };
        *value = Some(phase);
        chat.handle_server_notification(
            ServerNotification::ItemCompleted(message),
            /*replay_kind*/ None,
        );
        rendered.push(drain_insert_history(&mut rx));
    }
    assert_eq!(rendered[0].len(), 1);
    assert_eq!(rendered, vec![rendered[0].clone(); 3]);
}

#[tokio::test]
async fn replayed_notification_and_full_history_preserve_completed_items() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let message = completed_message("turn-1", "msg-1");
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message.clone()),
        /*replay_kind*/ None,
    );
    assert_eq!(drain_insert_history(&mut rx).len(), 1);

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message.clone()),
        Some(ReplayKind::ResumeInitialMessages),
    );
    let replayed = drain_insert_history(&mut rx);
    assert_eq!(replayed.len(), 1);

    let mut turn = completed_turn(&message).turn;
    turn.items_view = codex_app_server_protocol::TurnItemsView::Full;
    chat.replay_thread_turns(vec![turn], ReplayKind::ResumeInitialMessages);
    assert_eq!(drain_insert_history(&mut rx), replayed);
}

#[tokio::test]
async fn root_delivery_receipt_does_not_replace_last_authored_completion_identity() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let message = completed_message("turn-1", "msg-1");
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message.clone()),
        /*replay_kind*/ None,
    );
    assert_eq!(drain_insert_history(&mut rx).len(), 1);

    let receipt = codex_protocol::protocol::agent_delivery_receipt_item(
        ThreadId::new(),
        ThreadId::new(),
        MessagePhase::FinalAnswer,
        codex_protocol::protocol::SubAgentCompletionModelVisibility::NotVisible,
        codex_protocol::protocol::new_sub_agent_completion_context_response_item_id().as_str(),
        "Received marker.",
    )
    .expect("receipt");
    let mut receipt_notification = message.clone();
    receipt_notification.item =
        AppServerThreadItem::from(codex_protocol::items::TurnItem::AgentMessage(receipt));
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(receipt_notification),
        /*replay_kind*/ None,
    );
    assert_eq!(drain_insert_history(&mut rx).len(), 1);
    assert_eq!(
        chat.transcript.last_completed_agent_message,
        Some(("turn-1".to_string(), "msg-1".to_string()))
    );
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(message),
        /*replay_kind*/ None,
    );
    assert!(
        rx.try_recv().is_err(),
        "receipt must not defeat completion deduplication"
    );
}
