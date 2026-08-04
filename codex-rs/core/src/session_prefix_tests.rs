use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::format_inter_agent_completion_message;
use super::format_subagent_notification_message;

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
    let message = format_subagent_notification_message(
        "/root/worker",
        ThreadId::new(),
        &AgentStatus::Completed(Some(final_answer.clone())),
    );

    assert!(message.contains(&final_answer));
}
