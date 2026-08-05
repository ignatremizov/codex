use pretty_assertions::assert_eq;

use super::SubAgentCompletionStatus;
use super::is_sub_agent_completion_context_response_item_id;
use super::new_sub_agent_completion_context_response_item_id;
use super::new_sub_agent_completion_response_item_id;
use super::ordinary_agent_message_response_item_id;
use super::sub_agent_completion_item;
use super::sub_agent_completion_status_from_response_item_id;
use super::sub_agent_completion_transcript;
use super::sub_agent_completion_transcript_parts;
use crate::protocol::AgentStatus;

#[test]
fn completion_ids_preserve_terminal_status() {
    for status in [
        SubAgentCompletionStatus::Completed,
        SubAgentCompletionStatus::Errored,
        SubAgentCompletionStatus::Shutdown,
        SubAgentCompletionStatus::NotFound,
    ] {
        let id = new_sub_agent_completion_response_item_id(status);

        assert!(id.starts_with(&format!("msg_{}_", status.as_id_segment())));
        assert!(id.len() <= 64, "{id}");
        assert_eq!(
            sub_agent_completion_status_from_response_item_id(&id),
            Some(status)
        );
    }
}

#[test]
fn completion_status_rejects_ordinary_or_malformed_response_item_ids() {
    for id in [
        "msg_c_not-a-uuid",
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
    assert!(item.as_legacy_events().is_empty());

    let mut untrusted = item;
    untrusted.sub_agent_completion = None;
    assert!(!untrusted.has_sub_agent_completion_identity());
    assert_eq!(
        ordinary_agent_message_response_item_id(&untrusted.id),
        format!("agent_{}", untrusted.id)
    );
    assert!(!untrusted.as_legacy_events().is_empty());
}
