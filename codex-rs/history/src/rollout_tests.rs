use super::*;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn marker(start: u64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 0,
        materialized_turns: None,
        rollback_start_index: Some(start),
    }))
}

fn started(id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: id.into(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }))
}

fn completed(id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: id.into(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

#[test]
fn cumulative_overlapping_ranges_remove_union() {
    let items = vec![
        message("keep"),
        message("remove-a"),
        marker(1),
        message("remove-b"),
        marker(0),
        message("keep-after"),
    ];
    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![true, true, true, true, true, false]
    );
}

#[test]
fn terminal_evidence_for_surviving_turn_is_retained() {
    let items = vec![
        started("turn-1"),
        message("keep"),
        message("remove"),
        completed("turn-1"),
        marker(2),
    ];
    assert_eq!(
        rollout_without_exact_rollback_ranges(&items),
        vec![started("turn-1"), message("keep"), completed("turn-1")]
    );
}

#[test]
fn later_overlapping_marker_does_not_resurrect_removed_prefix() {
    let items = vec![message("gone"), marker(0), message("later"), marker(1)];
    assert_eq!(
        rollout_without_exact_rollback_ranges(&items),
        Vec::<RolloutItem>::new()
    );
}

#[test]
fn unknown_explicit_abort_does_not_consume_active_turn_evidence() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: Some("missing".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let items = vec![
        started("active"),
        RolloutItem::EventMsg(EventMsg::TurnAborted(event)),
        completed("active"),
        marker(1),
    ];
    assert_eq!(
        rollout_without_exact_rollback_ranges(&items),
        vec![started("active"), completed("active")]
    );
}

#[test]
fn explicit_abort_settles_the_active_turn_before_an_unattributed_abort() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: Some("active".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let unattributed = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: None,
        ..event.clone()
    };
    let items = vec![
        started("active"),
        RolloutItem::EventMsg(EventMsg::TurnAborted(event.clone())),
        RolloutItem::EventMsg(EventMsg::TurnAborted(unattributed)),
        marker(1),
    ];
    assert_eq!(
        rollout_without_exact_rollback_ranges(&items),
        vec![
            started("active"),
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
        ]
    );
}
