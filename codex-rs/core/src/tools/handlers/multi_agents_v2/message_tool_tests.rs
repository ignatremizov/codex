use super::*;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use crate::tools::context::ToolCallSource;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use pretty_assertions::assert_eq;

fn paths() -> (AgentPath, AgentPath) {
    (
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
    )
}

#[test]
fn communication_builder_honors_delivery_mode() {
    let (author, recipient) = paths();
    let encrypted = prepare_agent_message(
        "opaque".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::Encrypted,
        &ToolCallSource::Direct,
    )
    .expect("encrypted message")
    .into_communication(
        author.clone(),
        recipient.clone(),
        MessageDeliveryMode::TriggerTurn,
    );
    assert_eq!(
        encrypted,
        InterAgentCommunication::new_encrypted(
            author.clone(),
            recipient.clone(),
            Vec::new(),
            "opaque".to_string(),
            /*trigger_turn*/ true,
        )
    );

    let audit = "inspect the repository completely; ".repeat(100);
    let encrypted_with_audit = prepare_agent_message(
        "opaque".to_string(),
        Some(audit.clone()),
        MultiAgentMessageDelivery::EncryptedWithAudit,
        &ToolCallSource::Direct,
    )
    .expect("encrypted message with audit")
    .into_communication(
        author.clone(),
        recipient.clone(),
        MessageDeliveryMode::TriggerTurn,
    );
    let mut expected_encrypted_with_audit = InterAgentCommunication::new_encrypted(
        author.clone(),
        recipient.clone(),
        Vec::new(),
        "opaque".to_string(),
        /*trigger_turn*/ true,
    );
    expected_encrypted_with_audit.content = audit;
    assert_eq!(encrypted_with_audit, expected_encrypted_with_audit);

    let plaintext = prepare_agent_message(
        "inspect the repository".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::Plaintext,
        &ToolCallSource::Direct,
    )
    .expect("plaintext message")
    .into_communication(
        author.clone(),
        recipient.clone(),
        MessageDeliveryMode::TriggerTurn,
    );
    assert_eq!(
        plaintext,
        InterAgentCommunication::new(
            author.clone(),
            recipient.clone(),
            Vec::new(),
            InterAgentMessage::new(
                InterAgentMessageType::NewTask,
                recipient,
                author,
                "inspect the repository".to_string(),
            )
            .render(),
            /*trigger_turn*/ true,
        )
    );
}

#[test]
fn visible_content_exposes_plaintext_and_audit_messages_only() {
    let encrypted = prepare_agent_message(
        "opaque".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::Encrypted,
        &ToolCallSource::Direct,
    )
    .expect("encrypted message");
    let encrypted_with_audit = prepare_agent_message(
        "opaque".to_string(),
        Some("inspect the repository".to_string()),
        MultiAgentMessageDelivery::EncryptedWithAudit,
        &ToolCallSource::Direct,
    )
    .expect("encrypted message with audit");
    let plaintext = prepare_agent_message(
        "inspect the repository".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::Plaintext,
        &ToolCallSource::Direct,
    )
    .expect("plaintext message");

    assert_eq!(encrypted.visible_content(), None);
    assert_eq!(
        encrypted_with_audit.visible_content(),
        Some("inspect the repository")
    );
    assert_eq!(plaintext.visible_content(), Some("inspect the repository"));
}

#[test]
fn encrypted_with_audit_requires_readable_content() {
    let error = prepare_agent_message(
        "opaque".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::EncryptedWithAudit,
        &ToolCallSource::Direct,
    )
    .expect_err("missing audit content should fail");

    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "task_message is required when message_delivery is encrypted_with_audit".to_string()
        )
    );
}

#[test]
fn modes_without_audit_reject_task_message() {
    for message_delivery in [
        MultiAgentMessageDelivery::Encrypted,
        MultiAgentMessageDelivery::Plaintext,
    ] {
        let error = prepare_agent_message(
            "message".to_string(),
            Some("unexpected audit".to_string()),
            message_delivery,
            &ToolCallSource::Direct,
        )
        .expect_err("task_message should be rejected");

        assert_eq!(
            error,
            FunctionCallError::RespondToModel(
                "task_message is only supported when message_delivery is encrypted_with_audit"
                    .to_string()
            )
        );
    }
}

#[test]
fn supplied_null_audit_is_not_treated_as_an_omitted_field() {
    let arguments = r#"{"target":"worker","message":"message","task_message":null}"#;
    assert!(serde_json::from_str::<SendMessageArgs>(arguments).is_err());
    assert!(serde_json::from_str::<FollowupTaskArgs>(arguments).is_err());
}

#[test]
fn trusted_direct_plaintext_keeps_its_representation_under_encrypted_configuration() {
    for delivery in [
        MultiAgentMessageDelivery::Encrypted,
        MultiAgentMessageDelivery::EncryptedWithAudit,
    ] {
        let prepared = prepare_agent_message(
            "trusted plaintext".to_string(),
            /*task_message*/ None,
            delivery,
            &ToolCallSource::DirectPlaintextMessage,
        )
        .expect("direct plaintext is not ciphertext");
        let (author, recipient) = paths();
        let communication = prepared.into_communication(
            author.clone(),
            recipient.clone(),
            MessageDeliveryMode::QueueOnly,
        );
        assert_eq!(
            communication,
            InterAgentCommunication::new(
                author.clone(),
                recipient.clone(),
                Vec::new(),
                InterAgentMessage::new(
                    InterAgentMessageType::Message,
                    recipient,
                    author,
                    "trusted plaintext".to_string(),
                )
                .render(),
                /*trigger_turn*/ false,
            )
        );
    }
}

#[test]
fn payload_limit_counts_utf8_bytes_without_truncation() {
    let at_limit = "é".repeat(MAX_AGENT_MESSAGE_PAYLOAD_BYTES / 2);
    assert!(
        prepare_agent_message(
            at_limit.clone(),
            /*task_message*/ None,
            MultiAgentMessageDelivery::Plaintext,
            &ToolCallSource::Direct,
        )
        .is_ok()
    );
    assert!(
        prepare_agent_message(
            format!("{at_limit}é"),
            /*task_message*/ None,
            MultiAgentMessageDelivery::Plaintext,
            &ToolCallSource::Direct,
        )
        .is_err()
    );
}

#[test]
fn delivery_rejects_oversized_message_payloads() {
    let oversized = "x".repeat(MAX_AGENT_MESSAGE_PAYLOAD_BYTES + 1);
    let plaintext_error = prepare_agent_message(
        oversized,
        /*task_message*/ None,
        MultiAgentMessageDelivery::Plaintext,
        &ToolCallSource::Direct,
    )
    .expect_err("oversized plaintext should be rejected");
    let encrypted_with_audit_error = prepare_agent_message(
        "opaque".to_string(),
        Some("x".repeat(MAX_AGENT_MESSAGE_PAYLOAD_BYTES)),
        MultiAgentMessageDelivery::EncryptedWithAudit,
        &ToolCallSource::Direct,
    )
    .expect_err("oversized combined payload should be rejected");

    assert_eq!(
        (plaintext_error, encrypted_with_audit_error,),
        (
            FunctionCallError::RespondToModel(
                "message payload must not exceed 8192 bytes".to_string()
            ),
            FunctionCallError::RespondToModel(
                "combined message and task_message payload must not exceed 8192 bytes".to_string()
            ),
        )
    );
}
