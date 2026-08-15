//! Source-side user control is durable transcript audit, not a model-turn completion.

use super::*;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::items::UserAgentControlItem;
use pretty_assertions::assert_eq;

#[test]
fn standalone_user_control_replays_once_without_inventing_a_model_turn() {
    let control = UserAgentControlItem::succeeded(UserAgentControlAction::Observe);
    let turn_id = control.id.clone();
    let item = TurnItem::UserAgentControl(control);
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: turn_id.clone(),
        item: item.clone(),
        started_at_ms: Some(10),
        completed_at_ms: 20,
    });
    let mut live = ThreadHistoryBuilder::new();
    live.handle_event(&event);
    live.handle_event(&event);
    let expected = vec![Turn {
        id: turn_id,
        items: vec![ThreadItem::from(item)],
        items_view: TurnItemsView::Full,
        error: None,
        status: TurnStatus::Completed,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }];
    assert_eq!(live.finish(), expected);
    assert_eq!(
        build_turns_from_rollout_items(&[
            RolloutItem::EventMsg(event.clone()),
            RolloutItem::EventMsg(event),
        ]),
        expected
    );
}

#[test]
fn user_control_on_an_active_turn_does_not_finish_the_source_answer() {
    let control = UserAgentControlItem::succeeded(UserAgentControlAction::Spawn);
    let item = TurnItem::UserAgentControl(control);
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "active-source".into(),
        root_turn_id: None,
        trace_id: None,
        started_at: Some(10),
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }));
    builder.handle_event(&EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "active-source".into(),
        item: item.clone(),
        started_at_ms: Some(11_000),
        completed_at_ms: 12_000,
    }));
    assert_eq!(
        builder.turn_snapshot("active-source"),
        Some(Turn {
            id: "active-source".into(),
            items: vec![ThreadItem::from(item)],
            items_view: TurnItemsView::Full,
            error: None,
            status: TurnStatus::InProgress,
            started_at: Some(10),
            completed_at: None,
            duration_ms: None,
        })
    );
}
