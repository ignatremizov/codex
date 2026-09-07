use super::*;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;

#[test]
fn raw_response_item_uses_persisted_turn_without_starting_it() {
    let mut state = ThreadState::default();
    let changes = state.track_current_turn_event(
        "event-turn",
        &EventMsg::RawResponseItem(RawResponseItemEvent {
            item: ResponseItem::AgentMessage {
                id: Some(ResponseItemId::with_suffix("amsg", "task")),
                author: "/root".to_string(),
                recipient: "/root/worker".to_string(),
                content: vec![AgentMessageInputContent::InputText {
                    text: "Inspect the repository.".to_string(),
                }],
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        turn_id: Some("turn-a".to_string()),
                        ..Default::default()
                    },
                ),
            },
        }),
    );
    let expected_item = codex_app_server_protocol::ThreadItem::AgentMessage {
        id: "amsg_task".to_string(),
        text: "Agent message from `/root`:\n\nInspect the repository.".to_string(),
        attribution: None,
        input: None,
        phase: Some(codex_protocol::models::MessagePhase::Commentary),
        memory_citation: None,
        delivery: None,
        questions: None,
        inter_agent_source: Some(codex_app_server_protocol::InterAgentMessageSource {
            author: "/root".into(),
            recipient: "/root/worker".into(),
        }),
    };

    assert_eq!(
        changes.changed_items,
        vec![codex_app_server_protocol::ThreadHistoryItemChange {
            turn_id: "turn-a".to_string(),
            item: expected_item,
            started_at_ms: None,
            completed_at_ms: None,
        }]
    );
    assert_eq!(
        state.active_turn_snapshot(),
        None,
        "history-only messages must not create a live turn"
    );
}

#[test]
fn failed_current_turn_remains_available_to_live_snapshots() {
    let mut state = ThreadState::default();
    state.track_current_turn_event(
        "turn-a",
        &EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-a".to_string(),
            root_turn_id: None,
            trace_id: None,
            started_at: Some(10),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        }),
    );
    state.track_current_turn_event(
        "turn-a",
        &EventMsg::Error(ErrorEvent {
            misalignment: None,
            message: "model failed".to_string(),
            codex_error_info: None,
        }),
    );

    assert_eq!(
        state.active_turn_snapshot(),
        Some(Turn {
            id: "turn-a".to_string(),
            items: Vec::new(),
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            status: TurnStatus::Failed,
            error: Some(TurnError {
                misalignment: None,
                message: "model failed".to_string(),
                codex_error_info: None,
                additional_details: None,
            }),
            started_at: Some(10),
            completed_at: None,
            duration_ms: None,
        })
    );
}

#[tokio::test]
async fn resume_pause_excludes_only_that_connection_from_typed_transcript() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let existing_connection = ConnectionId(7);
    let resuming_connection = ConnectionId(8);
    manager
        .connection_initialized(existing_connection, ConnectionCapabilities::default())
        .await;
    manager
        .connection_initialized(resuming_connection, ConnectionCapabilities::default())
        .await;
    assert!(
        manager
            .try_add_connection_to_thread(thread_id, existing_connection)
            .await
    );
    assert!(
        manager
            .try_add_connection_to_thread(thread_id, resuming_connection)
            .await
    );
    let delivery_permit = manager
        .acquire_typed_transcript_delivery_permit(thread_id)
        .await;
    let pause = manager.pause_typed_transcript_for_resume(thread_id, resuming_connection);
    tokio::pin!(pause);
    assert!(futures::poll!(pause.as_mut()).is_pending());
    drop(delivery_permit);
    let first_pause = pause.await.expect("connected");
    let second_pause = manager
        .pause_typed_transcript_for_resume(thread_id, resuming_connection)
        .await
        .expect("connected");

    assert_eq!(
        manager
            .typed_transcript_connection_ids(thread_id)
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        [existing_connection].into_iter().collect()
    );
    drop(first_pause);
    assert_eq!(
        manager
            .typed_transcript_connection_ids(thread_id)
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        [existing_connection].into_iter().collect()
    );
    drop(second_pause);
    assert_eq!(
        manager
            .typed_transcript_connection_ids(thread_id)
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        [existing_connection, resuming_connection]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn cancelled_pause_waiter_never_registers_suppression() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let connection = ConnectionId(7);
    manager
        .connection_initialized(connection, ConnectionCapabilities::default())
        .await;
    assert!(
        manager
            .try_add_connection_to_thread(thread_id, connection)
            .await
    );
    let delivery = manager
        .acquire_typed_transcript_delivery_permit(thread_id)
        .await;
    let mut pending = Box::pin(manager.pause_typed_transcript_for_resume(thread_id, connection));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    drop(pending);
    drop(delivery);
    assert_eq!(
        manager.typed_transcript_connection_ids(thread_id).await,
        vec![connection]
    );
}

#[tokio::test]
async fn disconnected_resume_lease_cannot_suppress_reconnected_subscriber() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let connection = ConnectionId(7);
    manager
        .connection_initialized(connection, ConnectionCapabilities::default())
        .await;
    // Resume pauses admission before the connection becomes a subscriber.
    let previous = manager
        .pause_typed_transcript_for_resume(thread_id, connection)
        .await
        .expect("connected");
    manager.remove_connection(connection).await;
    manager
        .connection_initialized(connection, ConnectionCapabilities::default())
        .await;
    assert!(
        manager
            .try_add_connection_to_thread(thread_id, connection)
            .await
    );
    assert_eq!(
        manager.typed_transcript_connection_ids(thread_id).await,
        vec![connection]
    );
    let current = manager
        .pause_typed_transcript_for_resume(thread_id, connection)
        .await
        .expect("reconnected");
    drop(previous);
    assert_eq!(
        manager.typed_transcript_connection_ids(thread_id).await,
        Vec::<ConnectionId>::new()
    );
    drop(current);
    assert_eq!(
        manager.typed_transcript_connection_ids(thread_id).await,
        vec![connection]
    );
}
