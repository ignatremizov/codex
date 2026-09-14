use super::*;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_thread_store::MailboxFinalSubscriptionState;
use serde_json::json;

#[test]
fn wait_receipt_requires_adjacent_exact_token_endpoints_and_turn() {
    let subscription = MailboxFinalSubscription {
        message_id: "accepted-token".into(),
        sender_thread_id: ThreadId::new(),
        receiver_thread_id: ThreadId::new(),
        acceptance_sequence: 1,
        state: MailboxFinalSubscriptionState::Bound,
        bound_turn_id: Some("target-turn".into()),
        receiver_lifecycle_epoch: 0,
        sender_lifecycle_epoch: 0,
    };
    let wait = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: subscription.sender_thread_id,
        turn_id: "observer-turn".into(),
        item: TurnItem::CollabAgentToolCall(
            serde_json::from_value(json!({
                "id": "wait-call",
                "tool": "wait",
                "status": "completed",
                "sender_thread_id": subscription.sender_thread_id,
                "completion_presentation_agent_ids": [subscription.receiver_thread_id],
            }))
            .unwrap(),
        ),
        started_at_ms: None,
        completed_at_ms: 1,
    }));
    let receipt_id = codex_protocol::ResponseItemId::new("amsg");
    let observation: AgentResponseObservation = serde_json::from_value(json!({
        "observer_thread_id": subscription.sender_thread_id,
        "target_thread_id": subscription.receiver_thread_id,
        "target_turn_id": "target-turn",
        "pending_commentary": false,
        "commentary_delivery": null,
        "baseline_final_delivery": "none",
        "final_delivery": "wake",
        "final_delivery_response_item_id": receipt_id,
        "committed_delivery_response_item_ids": [receipt_id],
        "mailbox_final_subscription_message_id": "accepted-token",
    }))
    .unwrap();
    let receipt = RolloutItem::AgentResponseObservation(observation.clone());
    assert!(wait_delivered_subscription(
        &[wait.clone(), receipt.clone()],
        &subscription
    ));
    assert!(!wait_delivered_subscription(
        std::slice::from_ref(&receipt),
        &subscription
    ));
    for (observer, target, turn, token) in [
        (
            ThreadId::new(),
            subscription.receiver_thread_id,
            "target-turn",
            "accepted-token",
        ),
        (
            subscription.sender_thread_id,
            ThreadId::new(),
            "target-turn",
            "accepted-token",
        ),
        (
            subscription.sender_thread_id,
            subscription.receiver_thread_id,
            "unrelated-turn",
            "accepted-token",
        ),
        (
            subscription.sender_thread_id,
            subscription.receiver_thread_id,
            "target-turn",
            "older-token",
        ),
    ] {
        let mut other = observation.clone();
        other.observer_thread_id = observer;
        other.target_thread_id = target;
        other.target_turn_id = Some(turn.into());
        other.mailbox_final_subscription_message_id = Some(token.into());
        assert!(!wait_delivered_subscription(
            &[wait.clone(), RolloutItem::AgentResponseObservation(other)],
            &subscription
        ));
    }
}
