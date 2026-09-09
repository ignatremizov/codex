use super::*;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::agent_delivery_receipt_item;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use pretty_assertions::assert_eq;

fn receipt(
    delivered_phase: MessagePhase,
    visibility: SubAgentCompletionModelVisibility,
) -> AgentMessageItem {
    agent_delivery_receipt_item(
        ThreadId::new(),
        ThreadId::new(),
        delivered_phase,
        visibility,
        new_sub_agent_completion_context_response_item_id().as_str(),
        "Main's answer.",
    )
    .expect("valid core delivery receipt")
}

fn expected_message(id: String, phase: Option<MessagePhase>) -> ThreadItem {
    ThreadItem::AgentMessage {
        id,
        text: "Main's answer.".to_string(),
        attribution: None,
        input: None,
        phase,
        memory_citation: None,
        delivery: None,
        questions: None,
    }
}

#[test]
fn core_delivery_receipts_preserve_identity_through_app_server_conversion() {
    for delivered_phase in [MessagePhase::Commentary, MessagePhase::FinalAnswer] {
        for visibility in [
            SubAgentCompletionModelVisibility::Visible,
            SubAgentCompletionModelVisibility::NotVisible,
        ] {
            let receipt = receipt(delivered_phase.clone(), visibility);
            let expected = expected_message(receipt.id.clone(), Some(MessagePhase::Commentary));
            assert_eq!(
                ThreadItem::from(CoreTurnItem::AgentMessage(receipt)),
                expected,
            );
        }
    }
}

#[test]
fn provider_sanitized_receipt_identity_stays_ordinary_after_conversion() {
    for phase in [
        Some(MessagePhase::Commentary),
        Some(MessagePhase::FinalAnswer),
        None,
    ] {
        let mut message = receipt(
            MessagePhase::FinalAnswer,
            SubAgentCompletionModelVisibility::NotVisible,
        );
        let expected_id = format!("agent_{}", message.id);
        // This is the same sanitizer used by core::event_mapping::parse_agent_message
        // at the provider ResponseItem -> TurnItem boundary, before this conversion.
        message.id = ordinary_agent_message_response_item_id(&message.id);
        message.phase = phase.clone();
        assert_eq!(
            ThreadItem::from(CoreTurnItem::AgentMessage(message)),
            expected_message(expected_id, phase),
        );
    }
}

#[test]
fn receipt_identity_requires_presentation_commentary_phase() {
    for phase in [Some(MessagePhase::FinalAnswer), None] {
        let mut message = receipt(
            MessagePhase::FinalAnswer,
            SubAgentCompletionModelVisibility::NotVisible,
        );
        let expected_id = format!("agent_{}", message.id);
        message.phase = phase.clone();
        assert_eq!(
            ThreadItem::from(CoreTurnItem::AgentMessage(message)),
            expected_message(expected_id, phase),
        );
    }
}
