use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;
use serde_json::Value;
use serde_json::json;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::format_inter_agent_completion_message;
use super::format_subagent_commentary_message;
use super::format_subagent_notification_message;
use crate::context::AgentContextIdentity;
use crate::context::ContextualUserFragment;
use crate::context::UserAgentTask;

fn v2_agent(agent_path: &str, agent_id: ThreadId) -> AgentContextIdentity {
    AgentContextIdentity::V2 {
        agent_id,
        agent_path: AgentPath::try_from(agent_path).expect("valid agent path"),
    }
}

fn fragment_json(fragment: &str) -> Value {
    let body_start = fragment.find('\n').expect("fragment should contain a body") + 1;
    let body_end = fragment
        .rfind('\n')
        .expect("fragment should contain a closing marker");
    serde_json::from_str(&fragment[body_start..body_end]).expect("fragment body should be JSON")
}

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
    let agent_id = ThreadId::new();
    let message = format_subagent_notification_message(
        v2_agent("/root/worker", agent_id),
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
        v2_agent("/root/worker", agent_id),
        &AgentStatus::Completed(Some(final_answer.clone())),
    );

    assert!(message.contains(&final_answer));
}

#[test]
fn v1_agent_context_fragments_use_ref_and_nickname() {
    let agent_id =
        ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad").expect("valid thread id");
    let agent = AgentContextIdentity::V1 {
        agent_id,
        agent_ref: Some(2),
        nickname: Some("Pascal".to_string()),
    };

    let task = UserAgentTask::new(agent.clone(), "Review the lifecycle change.").render();
    let commentary = format_subagent_commentary_message(
        agent.clone(),
        "turn-1",
        "msg-1",
        "I will inspect the lifecycle paths.",
    );
    let notification = format_subagent_notification_message(
        agent,
        &AgentStatus::Completed(Some("No findings.".to_string())),
    );

    assert_eq!(
        [
            fragment_json(&task),
            fragment_json(&commentary),
            fragment_json(&notification),
        ],
        [
            json!({
                "agent_id": agent_id,
                "ref": "2",
                "nickname": "Pascal",
                "task_preview": "Review the lifecycle change.",
            }),
            json!({
                "agent_id": agent_id,
                "ref": "2",
                "nickname": "Pascal",
                "turn_id": "turn-1",
                "item_id": "msg-1",
                "message": "I will inspect the lifecycle paths.",
            }),
            json!({
                "agent_id": agent_id,
                "ref": "2",
                "nickname": "Pascal",
                "status": {"completed": "No findings."},
            }),
        ],
    );
}
