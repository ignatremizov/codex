use super::*;
use codex_app_server_protocol::ItemCompletedNotification;
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
fn completed_snapshot_and_buffered_terminal_events_cannot_recreate_processes() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.set_turns(vec![turn("old", TurnStatus::InProgress)]);
    store.push_notification(ServerNotification::TurnCompleted(
        TurnCompletedNotification {
            thread_id: "thread".into(),
            turn: turn("old", TurnStatus::Completed),
        },
    ));
    assert_eq!(active_snapshot_turn(&store.snapshot()), None);
    store.push_notification(ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: "thread".into(),
        turn: turn("new", TurnStatus::InProgress),
    }));
    let active = active_snapshot_turn(&store.snapshot());
    assert_eq!(active.as_deref(), Some("new"));
    for (turn_id, expected) in [
        ("old", ReplayKind::ReplayOnlyThreadSnapshot),
        ("new", ReplayKind::ThreadSnapshot),
    ] {
        let event = ThreadBufferedEvent::Notification(Box::new(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                thread_id: "thread".into(),
                turn_id: turn_id.into(),
                completed_at_ms: 0,
                item: ThreadItem::UserMessage {
                    id: "item".into(),
                    client_id: None,
                    content: Vec::new(),
                },
            },
        )));
        assert_eq!(
            item_replay_kind(&event, active.as_deref(), ReplayKind::ThreadSnapshot),
            expected
        );
    }
}
