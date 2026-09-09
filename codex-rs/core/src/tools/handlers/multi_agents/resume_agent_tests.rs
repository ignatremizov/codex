use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn resume_projects_ready_identity_without_replaying_a_final_answer() {
    for status in [
        AgentStatus::PendingInit,
        AgentStatus::Completed(None),
        AgentStatus::Completed(Some("previous result".to_string())),
    ] {
        let result = ResumeAgentResult {
            status,
            adoption: None,
            agent_id: ThreadId::new(),
            agent_ref: Some("2".to_string()),
            nickname: Some("worker".to_string()),
            task_path: Some("/root/review".to_string()),
        };
        assert_eq!(
            result.code_mode_result(&ToolPayload::Function {
                arguments: "{}".to_string(),
            }),
            json!({
                "ref": "2",
                "nickname": "worker",
                "task_path": "/root/review",
                "status": "idle",
            })
        );
    }
}

#[test]
fn resume_preserves_error_and_uses_uuid_when_no_shortref_is_available() {
    let agent_id = ThreadId::new();
    let result = ResumeAgentResult {
        status: AgentStatus::Errored("runtime failed".to_string()),
        adoption: None,
        agent_id,
        agent_ref: None,
        nickname: None,
        task_path: None,
    };
    assert_eq!(
        serde_json::to_value(result.model_result()).expect("serialize resume projection"),
        json!({
            "agent_id": agent_id,
            "nickname": null,
            "task_path": null,
            "status": "errored",
            "error": "runtime failed",
        })
    );
}
