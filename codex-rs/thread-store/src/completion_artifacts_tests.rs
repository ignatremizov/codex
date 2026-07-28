use codex_protocol::models::ContentItem;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn rollback_and_source_boundaries_cannot_fabricate_context_provenance() {
    let id = new_sub_agent_completion_context_response_item_id();
    let response = ResponseItem::Message {
        id: Some(id.clone()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "done".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let item = RolloutItem::ResponseItem(response.clone().into());
    let marker = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 1,
        materialized_turns: None,
        rollback_start_index: Some(1),
    }));
    assert_eq!(
        context_item(&[vec![metadata.clone(), item.clone()]], &id).expect("pair"),
        Some(response)
    );
    for segments in [
        vec![vec![metadata.clone()], vec![item.clone()]],
        vec![vec![metadata.clone(), metadata, marker, item]],
    ] {
        assert!(matches!(
            context_item(&segments, &id),
            Err(ThreadStoreError::Conflict { .. })
        ));
    }
}

#[test]
fn duplicate_presentation_identity_cannot_hide_conflicting_payloads() {
    let event = codex_protocol::protocol::ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "completion".to_string(),
        item: codex_protocol::items::TurnItem::AgentMessage(
            codex_protocol::protocol::sub_agent_completion_item(
                "/root/worker",
                &codex_protocol::protocol::AgentStatus::Completed(Some("first".to_string())),
            )
            .expect("terminal"),
        ),
        started_at_ms: None,
        completed_at_ms: 1,
    };
    let mut different = event.clone();
    let codex_protocol::items::TurnItem::AgentMessage(message) = &mut different.item else {
        unreachable!("fixture");
    };
    message.content = vec![codex_protocol::items::AgentMessageContent::Text {
        text: "Agent final answer from `/root/worker`:\n\nchanged".to_string(),
    }];
    let segments = vec![vec![
        RolloutItem::EventMsg(EventMsg::ItemCompleted(different)),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event.clone())),
    ]];
    assert!(matches!(
        presentation(&segments, &event.item.id(), &event.turn_id),
        Err(ThreadStoreError::Conflict { .. }),
    ));
}
