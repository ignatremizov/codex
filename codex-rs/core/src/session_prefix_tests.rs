use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
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
fn legacy_completion_notification_keeps_full_final_answer_in_its_user_fragment() {
    let answer = "long child answer ".repeat(2_000);
    let rendered = format_subagent_notification_message(
        "/root/worker",
        &AgentStatus::Completed(Some(answer.clone())),
    );
    let body = rendered
        .strip_prefix("<subagent_notification>")
        .and_then(|body| body.strip_suffix("</subagent_notification>"))
        .expect("legacy notification envelope");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("notification payload"),
        serde_json::json!({
            "agent_path": "/root/worker",
            "status": AgentStatus::Completed(Some(answer)),
        }),
    );
}
