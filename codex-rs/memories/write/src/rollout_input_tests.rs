use super::*;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use pretty_assertions::assert_eq;

#[test]
fn observation_control_records_do_not_become_memory_evidence() -> anyhow::Result<()> {
    let message = RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "Keep the original behavioral assertions.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id: ThreadId::new(),
        target_thread_id: ThreadId::new(),
        target_turn_id: Some("observed-turn".to_string()),
        task_preview: Some("Control-plane task preview, not memory evidence.".to_string()),
        promoted_task_context: None,
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
        mailbox_final_subscription_message_id: None,
        mailbox_final_subscription_suppressed_message_id: None,
    });

    assert_eq!(
        serialize_tiered_input(
            &[observation.clone(), message.clone(), observation],
            /*token_limit*/ 1_000,
        )?,
        serialize_tiered_input(&[message], /*token_limit*/ 1_000)?,
    );
    Ok(())
}

#[test]
fn extraction_chunks_preserve_unicode_evidence_with_bounded_messages() {
    let evidence = "User correction: 🐈\n".repeat(2_000);
    let mut reconstructed = String::new();
    for message in extraction_messages(&evidence) {
        let ResponseItem::Message { role, content, .. } = message else {
            panic!("message")
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("text")
        };
        assert!(text.len() < 9_000);
        reconstructed.push_str(text);
    }
    assert_eq!(reconstructed, evidence);
}

#[test]
fn classifies_memory_excluded_fragments() {
    let cases = [
        (
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
            true,
        ),
        (
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            false,
        ),
        (
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
            false,
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(
            is_memory_excluded_contextual_user_fragment(&ContentItem::InputText {
                text: text.to_string(),
            }),
            expected,
            "{text}",
        );
    }
}
