use super::*;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartedNotification;
use pretty_assertions::assert_eq;

fn turn(id: &str, status: TurnStatus) -> Turn {
    Turn {
        id: id.into(),
        items: Vec::new(),
        items_view: TurnItemsView::Full,
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

#[test]
fn partial_snapshot_is_not_trusted_until_canonical_read_certifies_it() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.set_turns(vec![turn("retained", TurnStatus::Completed)]);
    assert_eq!(materialized_thread_turns(&store), None);
    store.turn_history_complete = true;
    assert_eq!(materialized_thread_turns(&store), Some(store.turns.clone()));
}

#[test]
fn lifecycle_materialization_and_eviction_preserve_the_authoritative_boundary() {
    let mut store = ThreadEventStore::new(/*capacity*/ 2);
    store.set_turns(vec![turn("first", TurnStatus::Completed)]);
    store.turn_history_complete = true;
    store.push_notification(ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: "thread".into(),
        turn: turn("second", TurnStatus::InProgress),
        agent_queue: None,
    }));
    store.push_notification(ServerNotification::TurnCompleted(
        TurnCompletedNotification {
            thread_id: "thread".into(),
            turn: turn("second", TurnStatus::Completed),
        },
    ));
    assert_eq!(
        materialized_thread_turns(&store),
        Some(vec![
            turn("first", TurnStatus::Completed),
            turn("second", TurnStatus::Completed),
        ])
    );
    store.push_notification(ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: "thread".into(),
        turn: turn("third", TurnStatus::InProgress),
        agent_queue: None,
    }));
    assert_eq!(materialized_thread_turns(&store), None);
}
