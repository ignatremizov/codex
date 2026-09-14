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
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
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
    assert_eq!(
        delivered_final_turns_in_history(&history[2..], observer, child),
        HashSet::new(),
    );
    let mut uncommitted = receipt.clone();
    uncommitted.committed_delivery_response_item_ids.clear();
    assert_eq!(
        delivered_final_turns_in_history(
            &[
                history[0].clone(),
                history[1].clone(),
                RolloutItem::AgentResponseObservation(uncommitted),
            ],
            observer,
            child,
        ),
        HashSet::new(),
    );
    let mut detached = history.clone();
    detached.insert(
        /*index*/ 2,
        RolloutItem::EventMsg(EventMsg::ShutdownComplete),
    );
    assert_eq!(
        delivered_final_turns_in_history(&detached, observer, child),
        HashSet::new(),
        "unrelated records cannot connect a response to a later receipt",
    );
    let mut conflicting = history.clone();
    let RolloutItem::ResponseItem(mut duplicate) = history[1].clone() else {
        panic!("canonical response");
    };
    let ResponseItem::AgentMessage { content, .. } = &mut duplicate.item else {
        panic!("agent response");
    };
    *content = vec![
        codex_protocol::models::AgentMessageInputContent::InputText {
            text: "another payload for the same identity".to_owned(),
        },
    ];
    conflicting.push(RolloutItem::ResponseItem(duplicate));
    assert_eq!(
        delivered_final_turns_in_history(&conflicting, observer, child),
        HashSet::new(),
        "conflicting payloads cannot prove an exact delivery",
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

#[test]
fn current_reconciliation_suppresses_only_exact_committed_completed_turns() {
    for status in [
        AgentStatus::Completed(Some("same final".to_owned())),
        AgentStatus::Completed(None),
        AgentStatus::Errored("genuine error".to_owned()),
    ] {
        for delivered_final_turns in [
            HashSet::new(),
            HashSet::from(["finished-turn".to_owned()]),
            HashSet::from(["earlier-turn".to_owned()]),
        ] {
            let terminal = Some(("finished-turn".to_owned(), status.clone()));
            let snapshot = AgentResponseSnapshot {
                active_turn_id: None,
                last_terminal: terminal.clone(),
                next_event_sequence: 1,
                last_commentary_item_id: None,
                status: status.clone(),
            };
            let suppress = matches!(&status, AgentStatus::Completed(_))
                && delivered_final_turns.contains("finished-turn");
            let original = snapshot.clone();
            let start = ResponseObserverStart::CurrentOrNext {
                observed_status: AgentStatus::PendingInit,
                delivered_final_turns,
            };
            assert_eq!(
                start.reconciled_terminal(&snapshot),
                if suppress { None } else { terminal },
            );
            assert_eq!(snapshot, original, "filtering cannot change actual status");
        }
    }
}

#[test]
fn current_reconciliation_never_broadens_history_catch_up() {
    let status = AgentStatus::Completed(Some("historical final".to_owned()));
    let terminal = Some(("old-turn".to_owned(), status.clone()));
    for (start, active_turn_id, last_terminal, current) in [
        (
            ResponseObserverStart::FutureOnly,
            None,
            terminal.clone(),
            status.clone(),
        ),
        (
            ResponseObserverStart::CurrentOrNext {
                observed_status: status.clone(),
                delivered_final_turns: HashSet::new(),
            },
            None,
            terminal.clone(),
            status.clone(),
        ),
        (
            ResponseObserverStart::CurrentOrNext {
                observed_status: AgentStatus::PendingInit,
                delivered_final_turns: HashSet::new(),
            },
            Some("new-turn".to_owned()),
            terminal,
            AgentStatus::Running,
        ),
        (
            ResponseObserverStart::CurrentOrNext {
                observed_status: AgentStatus::PendingInit,
                delivered_final_turns: HashSet::new(),
            },
            None,
            None,
            AgentStatus::Completed(None),
        ),
    ] {
        let snapshot = AgentResponseSnapshot {
            active_turn_id,
            last_terminal,
            next_event_sequence: 1,
            last_commentary_item_id: None,
            status: current,
        };
        assert_eq!(start.reconciled_terminal(&snapshot), None);
    }
}

#[test]
fn durable_mailbox_token_reconciles_only_its_exact_bound_terminal() {
    let snapshot = AgentResponseSnapshot {
        active_turn_id: Some("new-running-turn".into()),
        last_terminal: Some((
            "bound-turn".into(),
            AgentStatus::Completed(Some("result".into())),
        )),
        next_event_sequence: 7,
        last_commentary_item_id: None,
        status: AgentStatus::Running,
    };
    assert_eq!(
        ResponseObserverStart::MailboxFinalTurn {
            turn_id: "bound-turn".into()
        }
        .reconciled_terminal(&snapshot),
        snapshot.last_terminal,
    );
    assert_eq!(
        ResponseObserverStart::MailboxFinalTurn {
            turn_id: "different-turn".into()
        }
        .reconciled_terminal(&snapshot),
        None,
    );
}
