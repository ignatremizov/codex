use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::GUARDIAN_NEXT_ACTION;
use super::format_guardian_interruption_message;
use super::format_inter_agent_completion_message;
use super::format_subagent_notification_message;
use pretty_assertions::assert_eq;

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    )
    .expect("error status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

#[test]
fn guardian_interruption_message_stays_below_manual_review_threshold() {
    let message = format_guardian_interruption_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &"Action is not authorized. ".repeat(1_000),
    );

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(GUARDIAN_NEXT_ACTION));
}

#[test]
fn errored_subagent_notification_stays_below_manual_review_threshold() {
    let message = format_subagent_notification_message(
        "/root/worker",
        ThreadId::new(),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    );

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains("stream disconnected"));
}

#[test]
fn successful_subagent_notification_preserves_the_complete_final_answer() {
    let final_answer = "complete child output ".repeat(2_000);
    let agent_id = ThreadId::new();
    let message = format_subagent_notification_message(
        "/root/worker",
        agent_id,
        &AgentStatus::Completed(Some(final_answer.clone())),
    );

    let body = message
        .strip_prefix("<subagent_notification>")
        .and_then(|body| body.strip_suffix("</subagent_notification>"))
        .expect("legacy notification envelope");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("notification payload"),
        serde_json::json!({
            "agent_path": "/root/worker",
            "agent_id": agent_id,
            "status": AgentStatus::Completed(Some(final_answer)),
        }),
    );
}
