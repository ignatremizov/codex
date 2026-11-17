use super::*;
use crate::AgentInputIdentity;
use crate::AgentPath;
use pretty_assertions::assert_eq;

fn prepared() -> PreparedAgentInput {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let mut communication = InterAgentCommunication::new_encrypted(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("worker path"),
        Vec::new(),
        "opaque-not-readable".to_string(),
        /*trigger_turn*/ true,
    );
    communication.content = "Readable audit: keep é, quotes, and every line.\n".repeat(180);
    PreparedAgentInput {
        communication,
        attribution: Box::new(AgentInputAttribution {
            sender: identity(sender),
            recipient: identity(recipient),
            sender_turn_id: "sender-turn".to_string(),
            batch_id: Some("original-batch".to_string()),
        }),
        sender_instance_id: uuid::Uuid::now_v7(),
        recipient_instance_id: uuid::Uuid::now_v7(),
        call_id: "original-tool-call".to_string(),
    }
}

#[test]
fn round_trip_retains_complete_payload_audit_and_original_identity() {
    let input = prepared();
    input
        .validate_receiver(input.attribution.recipient.thread_id)
        .expect("valid trusted input");
    let encoded = serde_json::to_vec(&input).expect("encode");
    let decoded: PreparedAgentInput = serde_json::from_slice(&encoded).expect("decode");
    assert_eq!(decoded, input);
    assert_eq!(
        decoded.readable_content(),
        Some(input.communication.content.as_str())
    );
}

#[test]
fn prepared_turn_input_round_trip_is_distinct_from_legacy_user_and_agent_shapes() {
    use crate::turn_input::TurnInput;
    let input = TurnInput::PreparedAgentInput(Box::new(prepared()));
    let encoded = serde_json::to_value(&input).expect("encode prepared input");
    assert!(encoded.get("PreparedAgentInput").is_some());
    assert!(encoded.get("UserInput").is_none());
    assert_eq!(
        serde_json::from_value::<TurnInput>(encoded).expect("decode prepared input"),
        input
    );

    // The added variant must not rewrite saved, pre-migration input shapes.
    for value in [
        serde_json::json!({"UserInput": {"content": [], "client_id": null}}),
        serde_json::json!({"AgentInput": {
            "content": [], "presentation": {"Attributed": "legacy agent"}
        }}),
    ] {
        let decoded: TurnInput = serde_json::from_value(value.clone()).expect("legacy input");
        assert_eq!(
            serde_json::to_value(decoded).expect("encode legacy input"),
            value
        );
    }
}

#[test]
fn encrypted_only_never_uses_ciphertext_as_readable_presentation() {
    let mut input = prepared();
    input.communication.content.clear();
    input
        .validate_receiver(input.attribution.recipient.thread_id)
        .expect("encrypted-only input");
    assert_eq!(input.readable_content(), None);
    assert!(matches!(
        input.communication.to_model_input_item(),
        crate::models::ResponseItem::AgentMessage { .. }
    ));
}

#[test]
fn receiver_identity_and_legacy_mail_semantics_cannot_be_substituted() {
    let input = prepared();
    let receiver = input.attribution.recipient.thread_id;
    assert!(input.validate_receiver(ThreadId::new()).is_err());
    let mut variants = vec![input; 7];
    variants[0].communication.trigger_turn = false;
    variants[1].communication.defer_to_next_turn = true;
    variants[2].communication.id = Some(crate::ResponseItemId::new("legacy"));
    variants[3].sender_instance_id = uuid::Uuid::nil();
    variants[4].call_id.clear();
    variants[5].communication.encrypted_content = Some("  ".to_string());
    variants[6]
        .communication
        .other_recipients
        .push(AgentPath::root());
    for invalid in variants {
        assert!(invalid.validate_receiver(receiver).is_err());
    }
}
