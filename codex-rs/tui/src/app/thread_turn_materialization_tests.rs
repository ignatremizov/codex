use super::*;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;

fn turn(id: &str, status: TurnStatus) -> Turn {
    Turn {
        id: id.to_string(),
        items_view: TurnItemsView::Full,
        items: Vec::new(),
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

#[test]
fn materializes_buffered_turn_items_and_completion() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.set_turns(vec![turn("turn-1", TurnStatus::Completed)]);
    store.push_notification(ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: "thread-1".to_string(),
        turn: turn("turn-2", TurnStatus::InProgress),
        agent_queue: None,
    }));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-2".to_string(),
            item: ThreadItem::UserMessage {
                id: "user-2".to_string(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: "second prompt".to_string(),
                    text_elements: Vec::new(),
                }],
            },
            completed_at_ms: 1,
        },
    ));
    store.push_notification(ServerNotification::TurnCompleted(
        TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: turn("turn-2", TurnStatus::Completed),
        },
    ));

    let mut expected_second = turn("turn-2", TurnStatus::Completed);
    expected_second.items = vec![ThreadItem::UserMessage {
        id: "user-2".to_string(),
        client_id: None,
        content: vec![UserInput::Text {
            text: "second prompt".to_string(),
            text_elements: Vec::new(),
        }],
    }];
    assert_eq!(
        materialized_thread_turns(&store),
        Some(vec![turn("turn-1", TurnStatus::Completed), expected_second])
    );
}

#[test]
fn declines_cached_turns_after_turn_lifecycle_eviction() {
    let mut store = ThreadEventStore::new(/*capacity*/ 1);
    store.set_turns(vec![turn("turn-1", TurnStatus::Completed)]);
    store.push_notification(ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: "thread-1".to_string(),
        turn: turn("turn-2", TurnStatus::InProgress),
        agent_queue: None,
    }));
    store.push_notification(ServerNotification::TurnCompleted(
        TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: turn("turn-2", TurnStatus::Completed),
        },
    ));

    assert_eq!(materialized_thread_turns(&store), None);
}
