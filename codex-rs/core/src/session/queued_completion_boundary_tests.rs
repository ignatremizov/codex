use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mcp_use_after_reference_context_persists_for_post_start_server() {
    let (sess, tc, _rx) = make_session_and_context_with_rx().await;
    let step_context = sess
        .capture_step_context(Arc::clone(&tc), &CancellationToken::new())
        .await
        .expect("capture step context");
    sess.record_context_updates_and_set_reference_context_item(&step_context)
        .await
        .expect("world state should build");

    sess.activate_mcp_server("linear".to_string()).await;

    assert_eq!(
        sess.latest_mcp_server_use_context_text("linear")
            .await
            .as_deref()
            .map(crate::context::McpServerUseInstructions::parse_server_name),
        Some(Some("linear".to_string())),
        "post-start MCP servers added by reload/plugin refresh must be persisted at the /mcp use point, not left in volatile pending state"
    );
    assert!(
        !sess
            .mcp_prompt
            .first_turn_servers
            .lock()
            .await
            .iter()
            .any(|name| name == "linear"),
        "persisted /mcp use context must not also remain queued"
    );
}

#[tokio::test]
async fn idle_interrupt_does_not_wake_queued_next_turn_items() {
    let (sess, _tc, _rx) = make_session_and_context_with_rx().await;
    let queued_item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::InputText {
            text: "queued before interrupt".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    sess.input_queue
        .queue_turn_inputs(vec![TurnInput::ResponseItem(queued_item.clone().into())])
        .await;

    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(sess.active_turn.lock().await.is_none());
    assert_eq!(
        sess.input_queue.queued_turn_inputs().await,
        vec![TurnInput::ResponseItem(queued_item.into())]
    );
}

#[tokio::test]
async fn abort_empty_active_turn_preserves_pending_input() {
    let (sess, _tc, _rx) = make_session_and_context_with_rx().await;
    let pending_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "late pending input".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let turn_state = {
        let mut active = sess.active_turn.lock().await;
        let active_turn = active.get_or_insert_with(ActiveTurn::default);
        Arc::clone(&active_turn.turn_state)
    };
    sess.input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(pending_item.clone().into())],
        )
        .await;

    sess.abort_all_tasks(TurnAbortReason::Replaced).await;

    assert!(sess.active_turn.lock().await.is_none());
    assert_eq!(
        sess.input_queue
            .take_pending_input_for_turn_state(turn_state.as_ref())
            .await,
        vec![TurnInput::ResponseItem(pending_item.into())]
    );
}
