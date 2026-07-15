use super::*;
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
    let encrypted = PreparedAgentMessage::from_tool_args(
        "opaque".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::Encrypted,
    )
    .expect("encrypted message")
    .into_communication(author.clone(), recipient.clone());
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

    let encrypted_with_audit = PreparedAgentMessage::from_tool_args(
        "opaque".to_string(),
        Some("inspect the repository".to_string()),
        MultiAgentMessageDelivery::EncryptedWithAudit,
    )
    .expect("encrypted message with audit")
    .into_communication(author.clone(), recipient.clone());
    let mut expected_encrypted_with_audit = InterAgentCommunication::new_encrypted(
        author.clone(),
        recipient.clone(),
        Vec::new(),
        "opaque".to_string(),
        /*trigger_turn*/ true,
    );
    expected_encrypted_with_audit.content = "inspect the repository".to_string();
    assert_eq!(encrypted_with_audit, expected_encrypted_with_audit);

    let plaintext = PreparedAgentMessage::from_tool_args(
        "inspect the repository".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::Plaintext,
    )
    .expect("plaintext message")
    .into_communication(author.clone(), recipient.clone());
    assert_eq!(
        plaintext,
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            "inspect the repository".to_string(),
            /*trigger_turn*/ true,
        )
    );
}

#[test]
fn encrypted_with_audit_requires_readable_content() {
    let error = PreparedAgentMessage::from_tool_args(
        "opaque".to_string(),
        /*task_message*/ None,
        MultiAgentMessageDelivery::EncryptedWithAudit,
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
        let error = PreparedAgentMessage::from_tool_args(
            "message".to_string(),
            Some("unexpected audit".to_string()),
            message_delivery,
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
