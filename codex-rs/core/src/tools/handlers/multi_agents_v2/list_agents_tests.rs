use super::*;
use codex_protocol::models::FunctionCallOutputBody;
use pretty_assertions::assert_eq;
use serde_json::json;

fn model_value(result: &ListAgentsResult, payload: &ToolPayload) -> JsonValue {
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        result.to_response_item("roster-call", payload)
    else {
        panic!("function result expected");
    };
    assert_eq!(output.success, Some(true));
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("JSON text expected");
    };
    let direct: JsonValue = serde_json::from_str(&text).expect("direct roster projection");
    assert_eq!(direct, result.code_mode_result(payload));
    direct
}

#[test]
fn roster_uses_one_compact_lifecycle_vocabulary_without_mutating_log_evidence() {
    let statuses = [
        AgentStatus::PendingInit,
        AgentStatus::Running,
        AgentStatus::Completed(Some("canonical final payload".to_string())),
        AgentStatus::Completed(None),
        AgentStatus::Interrupted,
        AgentStatus::Errored("canonical failure payload".to_string()),
        AgentStatus::Shutdown,
        AgentStatus::NotFound,
    ];
    let expected = [
        "pending_init",
        "running",
        "completed",
        "completed",
        "interrupted",
        "errored",
        "closed",
        "unloaded",
    ];
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    for (status, label) in statuses.into_iter().zip(expected) {
        let result = ListAgentsResult {
            agents: vec![ListedAgent {
                agent_name: "/root/worker".to_string(),
                agent_status: status.clone(),
                last_task_message: Some("Complete authored task, with 🦀 Unicode".to_string()),
            }],
        };
        let logged_before = result.log_output();
        let direct = model_value(&result, &payload);
        assert_eq!(
            direct,
            json!({"agents": [{
                "agent_name": "/root/worker",
                "agent_status": label,
                "last_task_message": "Complete authored task, with 🦀 Unicode",
            }]})
        );
        assert_eq!(
            serde_json::from_str::<JsonValue>(&logged_before).expect("full log"),
            json!({"agents": [{
                "agent_name": "/root/worker",
                "agent_status": status,
                "last_task_message": "Complete authored task, with 🦀 Unicode",
            }]})
        );
        assert_eq!(result.log_output(), logged_before);
        assert_eq!(
            serde_json::to_value(crate::agent::control::AgentDirectoryEntryStatus::from(
                &result.agents[0].agent_status
            ))
            .unwrap(),
            json!(label)
        );
    }
}

#[test]
fn model_roster_size_is_independent_of_repeated_completion_and_error_bodies() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let roster = |body: String| ListAgentsResult {
        agents: vec![
            ListedAgent {
                agent_name: "/root/finished".to_string(),
                agent_status: AgentStatus::Completed(Some(body.clone())),
                last_task_message: Some("Original task remains visible".to_string()),
            },
            ListedAgent {
                agent_name: "/root/failed".to_string(),
                agent_status: AgentStatus::Errored(body),
                last_task_message: None,
            },
        ],
    };
    let small = roster("short".to_string());
    let full_body = "complete response, no truncation 🦀\n".repeat(4096);
    let large = roster(full_body.clone());
    let small_direct = model_value(&small, &payload);
    let large_direct = model_value(&large, &payload);
    assert_eq!(small_direct, large_direct);
    let model_bytes = serde_json::to_vec(&large_direct).expect("model bytes");
    let log_bytes = large.log_output();
    assert!(
        model_bytes.len() <= 512,
        "two-row envelope budget exceeded: {} UTF-8 bytes",
        model_bytes.len()
    );
    let full_log: JsonValue = serde_json::from_str(&log_bytes).expect("full log");
    assert_eq!(
        full_log["agents"][0]["agent_status"]["completed"],
        full_body
    );
    assert_eq!(full_log["agents"][1]["agent_status"]["errored"], full_body);
    assert!(log_bytes.len() > full_body.len() * 2);
    tracing::info!(
        utf8_bytes = model_bytes.len(),
        full_audit_bytes = log_bytes.len(),
        authored_payload_bytes = full_body.len() * 2,
        "roster-envelope-v2"
    );
}
