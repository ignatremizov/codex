use super::*;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn receipt_requires_exact_endpoints_turn_and_persisted_final() {
    let observer = ThreadId::new();
    let child = ThreadId::new();
    let final_id = "amsg_x_019fc1b4-78ea-7481-97ac-ff423900cc6a";
    let response: ResponseItem = serde_json::from_value(json!({
        "type": "agent_message",
        "id": final_id,
        "author": "/root/child",
        "recipient": "/root",
        "content": [{"type": "input_text", "text": ""}],
    }))
    .unwrap();
    let receipt: AgentResponseObservation = serde_json::from_value(json!({
        "observer_thread_id": observer,
        "target_thread_id": child,
        "target_turn_id": "first",
        "pending_commentary": false,
        "commentary_delivery": null,
        "baseline_final_delivery": "none",
        "final_delivery": "passive",
        "final_delivery_response_item_id": final_id,
        "committed_delivery_response_item_ids": [final_id],
    }))
    .unwrap();
    let mut history = vec![
        RolloutItem::ResponseItem(response.into()),
        RolloutItem::AgentResponseObservation(receipt.clone()),
    ];
    assert_eq!(
        delivered_final_turns_in_history(&history, observer, child),
        HashSet::from(["first".to_string()]),
    );
    assert_eq!(
        delivered_final_turns_in_history(&history, ThreadId::new(), child),
        HashSet::new(),
    );
    assert_eq!(
        delivered_final_turns_in_history(&history, observer, ThreadId::new()),
        HashSet::new(),
    );
    assert_eq!(
        delivered_final_turns_in_history(&history[1..], observer, child),
        HashSet::new(),
    );
    let mut uncommitted = receipt.clone();
    uncommitted.committed_delivery_response_item_ids.clear();
    assert_eq!(
        delivered_final_turns_in_history(
            &[
                history[0].clone(),
                RolloutItem::AgentResponseObservation(uncommitted),
            ],
            observer,
            child,
        ),
        HashSet::new(),
    );
    // Finished observation cleanup revokes the subscription, not historical delivery evidence.
    let mut cleanup = receipt;
    cleanup.final_delivery = AgentResponseFinalDelivery::None;
    cleanup.final_delivery_response_item_id = None;
    cleanup.committed_delivery_response_item_ids.clear();
    history.push(RolloutItem::AgentResponseObservation(cleanup));
    history.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: Some(0),
        },
    )));
    // Exact rollback preserves committed out-of-band delivery artifacts.
    assert_eq!(
        delivered_final_turns_in_history(&history, observer, child),
        HashSet::from(["first".to_string()]),
    );
}
