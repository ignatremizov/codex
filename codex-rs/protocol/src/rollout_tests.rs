use super::*;
use crate::config_types::ModeKind;
use crate::models::ContentItem;
use crate::models::ResponseItem;
use crate::protocol::ThreadRolledBackEvent;
use crate::protocol::TurnCompleteEvent;
use crate::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn exact_rollback(start: u64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        rollback_start_index: Some(start),
        ..Default::default()
    }))
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }))
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        started_at: None,
        last_agent_message: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

#[test]
fn exact_rollback_ranges_remove_nested_markers_in_linear_pass() {
    let items = vec![
        message("survives"),
        message("first removed"),
        message("nested removed"),
        exact_rollback(2),
        message("later removed"),
        exact_rollback(1),
        message("survives after marker"),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![false, true, true, true, true, true, false]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![message("survives"), message("survives after marker")])
            .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_preserves_terminal_event_for_retained_turn_prefix() {
    let items = vec![
        turn_started("turn-1"),
        message("initial prompt"),
        message("steer"),
        turn_complete("turn-1"),
        exact_rollback(2),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![false, false, true, false, true]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![
            turn_started("turn-1"),
            message("initial prompt"),
            turn_complete("turn-1"),
        ])
        .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_matches_late_terminal_events_by_turn_id() {
    let items = vec![
        turn_started("turn-1"),
        message("surviving prompt"),
        turn_started("turn-2"),
        message("removed prompt"),
        turn_complete("turn-1"),
        exact_rollback(2),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![false, false, true, true, false, true]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![
            turn_started("turn-1"),
            message("surviving prompt"),
            turn_complete("turn-1"),
        ])
        .expect("serialize expected rollout")
    );
}
