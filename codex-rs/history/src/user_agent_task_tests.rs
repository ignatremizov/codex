use super::*;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use pretty_assertions::assert_eq;

fn task() -> ResponseItemEnvelope {
    ResponseItem::Message {
        id: Some(new_user_agent_task_context_response_item_id()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<user_agent_task>review the change</user_agent_task>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
    .into()
}

fn snapshot(item: &ResponseItemEnvelope) -> RolloutItem {
    RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id: ThreadId::new(),
        target_thread_id: ThreadId::new(),
        target_turn_id: Some("actual-turn".to_string()),
        task_preview: None,
        promoted_task_context: AgentResponsePromotedTaskContext::from_response_item(&item.item),
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        reply_route_enabled: None,
        reply_route_context_installed: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: None,
        committed_delivery_response_item_ids: Vec::new(),
    })
}

#[test]
fn exact_adjacent_task_payload_has_canonical_evidence() {
    let task = task();
    let snapshot = snapshot(&task);
    let records = [
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        RolloutItem::ResponseItem(task.clone()),
        snapshot.clone(),
    ];
    let proof = committed_user_agent_task_contexts(&records);
    let proof = proof.get(task.id().unwrap()).unwrap();
    assert_eq!(proof.item, task);
    assert_eq!(
        serde_json::to_value(RolloutItem::AgentResponseObservation(
            proof.observation.clone()
        ))
        .unwrap(),
        serde_json::to_value(snapshot).unwrap(),
    );
}

#[test]
fn matching_snapshot_requires_raw_adjacency_and_context_metadata() {
    let task = task();
    let snapshot = snapshot(&task);
    let response = RolloutItem::ResponseItem(task);
    for records in [
        vec![snapshot.clone()],
        vec![response.clone(), snapshot.clone()],
        vec![
            RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true },
            response.clone(),
            snapshot.clone(),
        ],
        vec![
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            },
            response,
            RolloutItem::Compacted(crate::CompactedItem::default()),
            snapshot,
        ],
    ] {
        assert!(committed_user_agent_task_contexts(&records).is_empty());
    }
}

#[test]
fn conflicting_checkpoint_payload_invalidates_task_identity() {
    let task = task();
    let mut conflict = task.clone();
    let ResponseItem::Message { content, .. } = &mut conflict.item else {
        unreachable!()
    };
    *content = vec![ContentItem::InputText {
        text: "<user_agent_task>different task</user_agent_task>".to_string(),
    }];
    let records = [
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        RolloutItem::ResponseItem(task.clone()),
        snapshot(&task),
        RolloutItem::Compacted(crate::CompactedItem {
            replacement_history: Some(vec![conflict]),
            ..Default::default()
        }),
    ];
    assert!(committed_user_agent_task_contexts(&records).is_empty());
}
