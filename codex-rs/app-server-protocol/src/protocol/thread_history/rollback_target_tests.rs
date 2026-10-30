use super::*;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;

#[test]
fn exact_item_cut_distinguishes_equal_steers_and_preserves_terminal_evidence() {
    let mut items = vec![RolloutItem::EventMsg(EventMsg::TurnStarted(
        TurnStartedEvent {
            turn_attribution: None,
            turn_id: "turn".into(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        },
    ))];
    for _ in 0..3 {
        items.push(RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                message: "same prompt".into(),
                ..Default::default()
            },
        )));
    }
    let terminal = RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: "turn".into(),
        started_at: None,
        last_agent_message: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));
    items.push(terminal.clone());
    let turns = super::super::build_turns_from_rollout_items(&items);
    let item_id = turns[0].items[2].id();
    let target = materialized_rollback_start(&items, /*num_turns*/ 1, Some(item_id))
        .expect("exact selected steer");
    assert_eq!(
        target,
        MaterializedRollbackStart {
            rollout_index: 3,
            turn_id: "turn".into(),
            turn_count: 1,
        },
    );
    items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: Some(target.rollout_index as u64),
        },
    )));
    assert_eq!(
        super::super::build_turns_from_rollout_items(&items),
        super::super::build_turns_from_rollout_items(&[
            items[0].clone(),
            items[1].clone(),
            items[2].clone(),
            terminal,
        ]),
    );
    assert_eq!(
        materialized_rollback_start(&items, /*num_turns*/ 1, Some(item_id)),
        None,
    );
}
