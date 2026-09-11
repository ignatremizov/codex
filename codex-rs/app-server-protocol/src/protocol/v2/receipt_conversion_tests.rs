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

#[test]
fn mailbox_acceptance_conversion_preserves_only_trusted_typed_receipts() {
    let id = codex_protocol::mailbox_acceptance_receipt_id(&ThreadId::new().to_string())
        .expect("acceptance receipt identity")
        .to_string();
    let sender = codex_protocol::AgentInputIdentity {
        thread_id: ThreadId::new(),
        nickname: Some("Kant".to_string()),
        agent_ref: Some("9".to_string()),
        task_path: Some("/root/task".to_string()),
        role: Some("explorer".to_string()),
        model: Some("model-at-send".to_string()),
        reasoning_effort: None,
    };
    let attribution = codex_protocol::AgentInputAttribution {
        sender,
        recipient: codex_protocol::AgentInputIdentity {
            thread_id: ThreadId::new(),
            nickname: Some("Hilbert".to_string()),
            agent_ref: Some("10".to_string()),
            task_path: Some("/root/other".to_string()),
            role: Some("reviewer".to_string()),
            model: None,
            reasoning_effort: None,
        },
        sender_turn_id: "source-turn".to_string(),
    };
    let input = vec![
        codex_protocol::user_input::UserInput::Text {
            text: "Read this later.".to_string(),
            text_elements: Vec::new(),
        },
        codex_protocol::user_input::UserInput::Image {
            image_url: "data:image/png;base64,original-bytes".to_string(),
            detail: None,
        },
    ];
    let mut message = AgentMessageItem::new(&[]);
    message.id = id.clone();
    message.phase = Some(MessagePhase::Commentary);
    message.attribution = Some(attribution.clone());
    message.input = Some(input.clone());
    let expected = ThreadItem::AgentMessage {
        id: id.clone(),
        text: String::new(),
        attribution: Some(attribution.into()),
        input: Some(input.into_iter().map(UserInput::from).collect()),
        phase: Some(MessagePhase::Commentary),
        memory_citation: None,
        delivery: None,
        questions: None,
    };
    assert_eq!(
        ThreadItem::from(CoreTurnItem::AgentMessage(message.clone())),
        expected,
    );
    enum Mutation {
        Provider,
        FinalPhase,
        MissingPhase,
        MissingAttribution,
        MissingInput,
        InvalidId,
    }
    for mutation in [
        Mutation::Provider,
        Mutation::FinalPhase,
        Mutation::MissingPhase,
        Mutation::MissingAttribution,
        Mutation::MissingInput,
        Mutation::InvalidId,
    ] {
        let mut candidate = message.clone();
        let mut expected = expected.clone();
        let ThreadItem::AgentMessage {
            id: expected_id,
            phase,
            attribution,
            input,
            ..
        } = &mut expected
        else {
            unreachable!();
        };
        *expected_id = format!("agent_{id}");
        match mutation {
            Mutation::Provider => {
                candidate.id = ordinary_agent_message_response_item_id(&id);
            }
            Mutation::FinalPhase => {
                candidate.phase = Some(MessagePhase::FinalAnswer);
                *phase = Some(MessagePhase::FinalAnswer);
            }
            Mutation::MissingPhase => {
                candidate.phase = None;
                *phase = None;
            }
            Mutation::MissingAttribution => {
                candidate.attribution = None;
                *attribution = None;
            }
            Mutation::MissingInput => {
                candidate.input = None;
                *input = None;
            }
            Mutation::InvalidId => {
                candidate.id = "msg_mailbox_accepted_invalid".to_string();
                *expected_id = candidate.id.clone();
            }
        }
        assert_eq!(
            ThreadItem::from(CoreTurnItem::AgentMessage(candidate)),
            expected,
        );
    }
}
