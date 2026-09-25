use super::super::build_legacy_api_turns_from_rollout_items;
use codex_history::RolloutItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;

#[test]
fn exact_steer_rollback_keeps_retained_turn_completed() {
    let items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
            agent_queue: None,
        })),
        user_message("initial prompt"),
        agent_message("initial answer"),
        user_message("steer"),
        agent_message("answer after steer"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            started_at: None,
            last_agent_message: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
            rollback_start_index: Some(3),
            materialized_turns: None,
        })),
    ];

    let turns = build_legacy_api_turns_from_rollout_items(&items);

    assert_eq!(
        turns,
        build_legacy_api_turns_from_rollout_items(&[
            items[0].clone(),
            items[1].clone(),
            items[2].clone(),
            items[5].clone(),
        ])
    );
}

fn user_message(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        client_id: None,
        message: message.to_string(),
        images: None,
        local_images: Vec::new(),
        text_elements: Vec::new(),
        ..Default::default()
    }))
}

fn agent_message(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: message.to_string(),
        phase: None,
        memory_citation: None,
        delivery: None,
        questions: None,
    }))
}
