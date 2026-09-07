use pretty_assertions::assert_eq;

use super::SubAgentCompletionModelVisibility;
use super::SubAgentCompletionStatus;
use super::is_attributed_agent_message_response_item_id;
use super::is_sub_agent_completion_context_response_item_id;
use super::is_user_agent_task_context_response_item_id;
use super::new_attributed_agent_message_response_item_id;
use super::new_sub_agent_completion_context_response_item_id;
use super::new_sub_agent_completion_response_item_id;
use super::new_user_agent_task_context_response_item_id;
use super::ordinary_agent_message_response_item_id;
use super::sub_agent_completion_item;
use super::sub_agent_completion_item_with_visibility;
use super::sub_agent_completion_model_visibility_from_response_item_id;
use super::sub_agent_completion_status_from_response_item_id;
use super::sub_agent_completion_transcript;
use super::sub_agent_completion_transcript_parts;
use crate::ResponseItemId;
use crate::models::MessagePhase;
use crate::protocol::AgentStatus;

#[test]
fn completion_ids_preserve_terminal_status() {
    for model_visibility in [
        SubAgentCompletionModelVisibility::Visible,
        SubAgentCompletionModelVisibility::NotVisible,
    ] {
        for status in [
            SubAgentCompletionStatus::Completed,
            SubAgentCompletionStatus::Errored,
            SubAgentCompletionStatus::Shutdown,
            SubAgentCompletionStatus::NotFound,
        ] {
            let id = new_sub_agent_completion_response_item_id(status, model_visibility);
            let prefix = match model_visibility {
                SubAgentCompletionModelVisibility::Visible => "msg",
                SubAgentCompletionModelVisibility::NotVisible => "msgx",
            };

            assert!(id.starts_with(&format!("{prefix}_{}_", status.as_id_segment())));
            assert!(id.len() <= 64, "{id}");
            assert_eq!(
                (
                    sub_agent_completion_status_from_response_item_id(&id),
                    sub_agent_completion_model_visibility_from_response_item_id(&id),
                ),
                (Some(status), Some(model_visibility))
            );
        }
    }
}

#[test]
fn completion_status_rejects_ordinary_or_malformed_response_item_ids() {
    for id in [
        "msg_c_not-a-uuid",
        "msgx_c_not-a-uuid",
        "msg_running_01900000-0000-7000-8000-000000000001",
        "msg_c_550e8400-e29b-41d4-a716-446655440000",
    ] {
        assert!(
            sub_agent_completion_status_from_response_item_id(id).is_none(),
            "{id}"
        );
    }
}

#[test]
fn completion_context_ids_are_distinct_and_validated() {
    let id = new_sub_agent_completion_context_response_item_id();

    assert!(id.starts_with("amsg_x_"));
    assert!(id.len() <= 64, "{id}");
    assert!(is_sub_agent_completion_context_response_item_id(&id));
    assert!(!is_sub_agent_completion_context_response_item_id(
        "amsg_x_not-a-uuid"
    ));
    assert!(!is_sub_agent_completion_context_response_item_id(
        "msg_x_not-a-uuid"
    ));
}

#[test]
fn user_agent_task_context_ids_are_distinct_and_validated() {
    let id = new_user_agent_task_context_response_item_id();

    assert!(id.starts_with("msg_t_"));
    assert!(id.len() <= 64, "{id}");
    assert!(is_user_agent_task_context_response_item_id(&id));
    assert!(!is_user_agent_task_context_response_item_id(
        "msg_t_not-a-uuid"
    ));
    assert!(!is_user_agent_task_context_response_item_id(
        "amsg_x_not-a-uuid"
    ));
}

#[test]
fn attributed_agent_message_ids_are_distinct_and_validated() {
    let id = new_attributed_agent_message_response_item_id();

    assert!(id.starts_with("msg_a_"));
    assert!(id.len() <= 64, "{id}");
    assert!(is_attributed_agent_message_response_item_id(&id));
    assert!(!is_attributed_agent_message_response_item_id(
        "msg_a_not-a-uuid"
    ));
    assert_eq!(
        ordinary_agent_message_response_item_id(&id),
        format!("agent_{id}")
    );
}

#[test]
fn canonical_completion_round_trips_and_skips_lossy_legacy_projection() {
    let (id, text) = sub_agent_completion_transcript(
        "/root/reviewer",
        &AgentStatus::Completed(Some("Finished reviewing.".to_string())),
    )
    .expect("terminal status");

    assert_eq!(
        sub_agent_completion_status_from_response_item_id(&id),
        Some(SubAgentCompletionStatus::Completed)
    );
    assert_eq!(
        sub_agent_completion_transcript_parts(&text),
        Some(("/root/reviewer", "Finished reviewing."))
    );
    let item = sub_agent_completion_item(
        "/root/reviewer",
        &AgentStatus::Completed(Some("Finished reviewing.".to_string())),
    )
    .expect("terminal status");

    assert!(item.has_sub_agent_completion_identity());
    assert_eq!(
        sub_agent_completion_model_visibility_from_response_item_id(&item.id),
        Some(SubAgentCompletionModelVisibility::Visible)
    );
    assert!(item.as_legacy_events().is_empty());

    let hidden_item = sub_agent_completion_item_with_visibility(
        "/root/reviewer",
        &AgentStatus::Completed(Some("Finished reviewing.".to_string())),
        SubAgentCompletionModelVisibility::NotVisible,
    )
    .expect("terminal status");
    assert!(hidden_item.has_sub_agent_completion_identity());
    assert_eq!(
        sub_agent_completion_model_visibility_from_response_item_id(&hidden_item.id),
        Some(SubAgentCompletionModelVisibility::NotVisible)
    );

    let mut untrusted = item;
    untrusted.sub_agent_completion = None;
    assert!(!untrusted.has_sub_agent_completion_identity());
    assert_eq!(
        ordinary_agent_message_response_item_id(&untrusted.id),
        format!("agent_{}", untrusted.id)
    );
    assert!(!untrusted.as_legacy_events().is_empty());

    let mut hidden_untrusted = hidden_item;
    hidden_untrusted.sub_agent_completion = None;
    assert!(!hidden_untrusted.has_sub_agent_completion_identity());
    assert_eq!(
        ordinary_agent_message_response_item_id(&hidden_untrusted.id),
        format!("agent_{}", hidden_untrusted.id)
    );
}

#[test]
fn model_context_completion_projects_with_a_stable_visible_identity() {
    let source_id = "amsg_01900000-0000-7000-8000-000000000001";
    let status = AgentStatus::Completed(Some("Finished reviewing.".to_string()));

    let first = super::sub_agent_completion_transcript_from_agent_message_id(
        source_id,
        "01900000-0000-7000-8000-000000000002",
        &status,
    )
    .expect("valid model-context completion");
    let second = super::sub_agent_completion_transcript_from_agent_message_id(
        source_id,
        "01900000-0000-7000-8000-000000000002",
        &status,
    )
    .expect("valid model-context completion");

    assert_eq!(first, second);
    assert_eq!(
        first,
        (
            ResponseItemId::from_server(
                "msg_c_01900000-0000-7000-8000-000000000001".to_string()
            ),
            "Agent final answer from `01900000-0000-7000-8000-000000000002`:\n\nFinished reviewing."
                .to_string(),
        )
    );
    assert!(
        super::sub_agent_completion_transcript_from_agent_message_id(
            "amsg_x_01900000-0000-7000-8000-000000000001",
            "01900000-0000-7000-8000-000000000002",
            &status,
        )
        .is_none()
    );
}
#[test]
fn delivery_receipt_identity_preserves_direction_phase_and_retry_identity() {
    use crate::ThreadId;

    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let delivery_id = super::new_sub_agent_completion_context_response_item_id();
    for (phase, visibility) in [
        (
            MessagePhase::Commentary,
            SubAgentCompletionModelVisibility::Visible,
        ),
        (
            MessagePhase::FinalAnswer,
            SubAgentCompletionModelVisibility::Visible,
        ),
        (
            MessagePhase::FinalAnswer,
            SubAgentCompletionModelVisibility::NotVisible,
        ),
    ] {
        let receipt = super::agent_delivery_receipt_item(
            sender,
            recipient,
            phase.clone(),
            visibility,
            delivery_id.as_str(),
            "Agent final answer from `/root/other`:\n\nuntrusted payload",
        )
        .expect("receipt");
        assert_eq!(
            super::agent_delivery_receipt_from_response_item_id(&receipt.id),
            Some((sender, recipient, phase.clone(), visibility))
        );
        assert_eq!(
            serde_json::to_value(super::agent_delivery_receipt_item(
                sender,
                recipient,
                phase.clone(),
                visibility,
                delivery_id.as_str(),
                "Agent final answer from `/root/other`:\n\nuntrusted payload",
            ))
            .expect("serialize retry"),
            serde_json::to_value(Some(receipt.clone())).expect("serialize receipt")
        );
        assert!(!receipt.is_attributed_agent_input_presentation());
        assert!(!receipt.has_sub_agent_completion_identity());
        assert_eq!(
            super::ordinary_agent_message_response_item_id(&receipt.id),
            format!("agent_{}", receipt.id)
        );
        let other_recipient = super::agent_delivery_receipt_item(
            sender,
            ThreadId::new(),
            phase,
            visibility,
            delivery_id.as_str(),
            "same delivery, different endpoint",
        )
        .expect("fan-out receipt");
        assert_ne!(receipt.id, other_recipient.id);
        assert_eq!(
            super::agent_delivery_receipt_from_response_item_id(&format!("{}_extra", receipt.id)),
            None
        );
    }
    assert!(
        super::agent_delivery_receipt_item(
            sender,
            recipient,
            MessagePhase::FinalAnswer,
            SubAgentCompletionModelVisibility::NotVisible,
            "ordinary-untrusted-id",
            "payload",
        )
        .is_none()
    );
}
