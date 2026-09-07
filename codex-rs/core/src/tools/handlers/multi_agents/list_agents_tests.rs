use super::*;
use crate::agent::control::AgentDirectoryEntry;
use crate::agent::control::AgentDirectoryEntryStatus;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn stale_directory_call_is_denied_even_when_turn_config_claims_enabled() {
    let (session, mut turn) = make_session_and_context().await;
    turn.multi_agent_version = MultiAgentVersion::V1;
    Arc::make_mut(&mut turn.config).list_agents_enabled = true;
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "stale-directory".to_string(),
        tool_name: Handler.tool_name(),
        source: ToolCallSource::Direct,
        // Check admission before parsing even a previously discovered call's arguments.
        payload: ToolPayload::Function {
            arguments: "{".to_string(),
        },
    };

    let error = Handler
        .handle(invocation)
        .await
        .err()
        .expect("directory admission must use the root, not stale turn configuration");
    assert_eq!(
        error,
        FunctionCallError::RespondToModel(
            "V1 list_agents is disabled. The user must explicitly enable \
             tools.list_agents.enabled; messaging permission does not enable discovery."
                .to_string()
        )
    );
}

#[test]
fn directory_result_preserves_identity_and_cursor_without_task_payloads() {
    let agent_id = ThreadId::new();
    let result = ListAgentsResult(AgentDirectoryPage {
        agents: vec![AgentDirectoryEntry {
            agent_id,
            agent_ref: "3".to_string(),
            nickname: Some("Pascal".to_string()),
            role: Some("coder".to_string()),
            task_path: Some("/root/backend/auth".to_string()),
            status: AgentDirectoryEntryStatus::Completed,
        }],
        next_cursor: Some("3".to_string()),
    });

    assert_eq!(
        serde_json::to_value(result).expect("serializable directory"),
        json!({
            "agents": [{
                "agent_id": agent_id,
                "ref": "3",
                "nickname": "Pascal",
                "role": "coder",
                "task_path": "/root/backend/auth",
                "status": "completed"
            }],
            "next_cursor": "3"
        })
    );
}

#[test]
fn directory_arguments_accept_filters_and_reject_invalid_calls() {
    for status in [
        "loaded",
        "running",
        "pending_init",
        "completed",
        "interrupted",
        "errored",
        "closed",
        "all",
    ] {
        let args: ListAgentsArgs = parse_arguments(
            &json!({
                "path_prefix": "backend/auth",
                "status": status,
                "cursor": "3",
                "limit": 2
            })
            .to_string(),
        )
        .expect("supported directory query");
        assert_eq!(
            (
                args.path_prefix.as_deref(),
                args.cursor.as_deref(),
                args.limit
            ),
            (Some("backend/auth"), Some("3"), Some(2))
        );
    }
    for arguments in [
        r#"{"status":"shutdown"}"#,
        r#"{"status":"unknown"}"#,
        r#"{"limit":-1}"#,
        r#"{"limit":1.5}"#,
        r#"{"unexpected":true}"#,
    ] {
        assert!(parse_arguments::<ListAgentsArgs>(arguments).is_err());
    }
}
