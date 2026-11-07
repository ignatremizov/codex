use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::McpServerElicitationRequest;
use codex_app_server_protocol::McpServerElicitationRequestResponse;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_config::config_toml::ConfigToml;
use codex_features::Feature;
use codex_protocol::mcp_approval_meta as approval_meta;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

use super::mcp_tool::start_mcp_server_with_tools;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_TOOLS: &[&str] = &["create_thread", "send_message_to_thread", "fork_thread"];

enum ToolApprovalChoice {
    AlwaysAllow,
    Decline,
}

enum StoredAllowance {
    Granted,
    Revoked,
    RevokedForSession,
}

#[test_case("create_thread"; "create")]
#[test_case("send_message_to_thread"; "message")]
#[test_case("fork_thread"; "fork")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_mcp_always_allow_survives_server_restart(tool_name: &str) -> Result<()> {
    let other_tool = if tool_name == "create_thread" {
        "send_message_to_thread"
    } else {
        "create_thread"
    };
    let mut responses = Vec::new();
    for (call_id, tool, message) in [
        ("first", tool_name, "first target"),
        ("restarted", tool_name, "different target"),
        ("other", other_tool, "another tool"),
    ] {
        responses.push(responses::sse(vec![
            responses::ev_response_created(call_id),
            responses::ev_function_call_with_namespace(
                call_id,
                "mcp__codex_tui",
                tool,
                &json!({"message": message}).to_string(),
            ),
            responses::ev_completed(call_id),
        ]));
        responses.push(create_final_assistant_message_sse_response("done")?);
    }
    let responses_server = create_mock_responses_server_sequence(responses).await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_approval_policy("on-request")
        .with_provider_config("supports_websockets = false")
        .enable_feature(Feature::ToolCallMcpElicitation)
        .write(codex_home.path())?;
    let (first_url, first_server) =
        start_mcp_server_with_tools(TASK_TOOLS, /*sensitive_action*/ None).await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(session_mcp_params(&first_url, "auto"))
        .await?
        .thread;
    start_tool_turn(&mut app, &thread.id).await?;
    resolve_tool_approval(&mut app, ToolApprovalChoice::AlwaysAllow).await?;
    assert_eq!(
        finish_tool_call(&mut app, "first").await?,
        (
            McpToolCallStatus::Completed,
            Some(json!([{"type": "text", "text": "echo: first target"}])),
        )
    );

    let saved = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    let parsed: ConfigToml = toml::from_str(&saved)?;
    assert_eq!(
        (parsed.mcp_servers, parsed.mcp_tool_approvals),
        (
            HashMap::new(),
            HashMap::from([(
                "codex_tui".to_string(),
                HashMap::from([(tool_name.to_string(), true)]),
            )]),
        )
    );
    // Parsing the entire file proves the choice did not create a partial server
    // registration; the live URL and authorization override must stay transient.
    assert!(!saved.contains(&first_url));
    assert!(!saved.contains("ephemeral-tui-token"));
    assert!(
        timeout(READ_TIMEOUT, app.shutdown_gracefully())
            .await??
            .success()
    );
    let (second_url, second_server) =
        start_mcp_server_with_tools(TASK_TOOLS, /*sensitive_action*/ None).await?;
    // Keep the old listener bound until the new one has a distinct address.
    assert_ne!(first_url, second_url);
    first_server.abort();
    let _ = first_server.await;
    let mut restarted = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = restarted
        .start_thread(session_mcp_params(&second_url, "auto"))
        .await?
        .thread;
    start_tool_turn(&mut restarted, &thread.id).await?;
    // An unexpected approval blocks completion and fails the bounded wait.
    assert_eq!(
        finish_tool_call(&mut restarted, "restarted").await?,
        (
            McpToolCallStatus::Completed,
            Some(json!([{"type": "text", "text": "echo: different target"}])),
        )
    );

    start_tool_turn(&mut restarted, &thread.id).await?;
    resolve_tool_approval(&mut restarted, ToolApprovalChoice::Decline).await?;
    assert_eq!(
        finish_tool_call(&mut restarted, "other").await?,
        (McpToolCallStatus::Failed, None)
    );
    assert!(
        timeout(READ_TIMEOUT, restarted.shutdown_gracefully())
            .await??
            .success()
    );
    second_server.abort();
    let _ = second_server.await;
    Ok(())
}

#[test_case("auto", StoredAllowance::Revoked; "revoked")]
#[test_case("auto", StoredAllowance::RevokedForSession; "session_revocation")]
#[test_case("prompt", StoredAllowance::Granted; "forced_prompt")]
#[test_case("writes", StoredAllowance::Granted; "forced_writes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_session_mcp_allowance_respects_revocation_and_forced_modes(
    mode: &str,
    allowance: StoredAllowance,
) -> Result<()> {
    let responses_server = create_mock_responses_server_sequence(vec![
        responses::sse(vec![
            responses::ev_response_created("blocked"),
            responses::ev_function_call_with_namespace(
                "blocked",
                "mcp__codex_tui",
                "send_message_to_thread",
                r#"{"message": "requires approval"}"#,
            ),
            responses::ev_completed("blocked"),
        ]),
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    let approved = !matches!(allowance, StoredAllowance::Revoked);
    MockResponsesConfig::new(&responses_server.uri())
        .with_approval_policy("on-request")
        .with_provider_config("supports_websockets = false")
        .enable_feature(Feature::ToolCallMcpElicitation)
        .with_extra_config(&format!(
            "[mcp_tool_approvals.codex_tui]\nsend_message_to_thread = {approved}"
        ))
        .write(codex_home.path())?;
    let (url, server) = start_mcp_server_with_tools(TASK_TOOLS, /*sensitive_action*/ None).await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let mut params = session_mcp_params(&url, mode);
    if matches!(allowance, StoredAllowance::RevokedForSession) {
        params.config.get_or_insert_default().insert(
            "mcp_tool_approvals.codex_tui.send_message_to_thread".to_string(),
            json!(false),
        );
    }
    let thread = app.start_thread(params).await?.thread;
    start_tool_turn(&mut app, &thread.id).await?;
    resolve_tool_approval(&mut app, ToolApprovalChoice::Decline).await?;
    assert_eq!(
        finish_tool_call(&mut app, "blocked").await?,
        (McpToolCallStatus::Failed, None)
    );
    assert!(
        timeout(READ_TIMEOUT, app.shutdown_gracefully())
            .await??
            .success()
    );
    server.abort();
    let _ = server.await;
    Ok(())
}

fn session_mcp_params(url: &str, mode: &str) -> ThreadStartParams {
    ThreadStartParams {
        config: Some(HashMap::from([(
            "mcp_servers.codex_tui".to_string(),
            json!({
                "url": format!("{url}/mcp"),
                "http_headers": {"Authorization": "Bearer ephemeral-tui-token"},
                "default_tools_approval_mode": mode,
            }),
        )])),
        ..Default::default()
    }
}

async fn start_tool_turn(app: &mut TestAppServer, thread_id: &str) -> Result<()> {
    let request = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: "Call the task tool".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(READ_TIMEOUT, app.read_response(request)).await??;
    Ok(())
}

async fn resolve_tool_approval(app: &mut TestAppServer, choice: ToolApprovalChoice) -> Result<()> {
    let request = timeout(READ_TIMEOUT, app.read_stream_until_request_message()).await??;
    let ServerRequest::McpServerElicitationRequest { request_id, params } = request else {
        bail!("expected a tool approval, got {request:?}");
    };
    let McpServerElicitationRequest::Form { meta, .. } = params.request else {
        bail!("expected a form approval");
    };
    assert_eq!(params.server_name, "codex_tui");
    let meta = meta.context("approval metadata")?;
    assert_eq!(
        meta.get(approval_meta::APPROVAL_KIND_KEY),
        Some(&json!(approval_meta::APPROVAL_KIND_MCP_TOOL_CALL))
    );
    let (action, response_meta) = match choice {
        ToolApprovalChoice::AlwaysAllow => {
            assert_eq!(
                meta.get(approval_meta::PERSIST_KEY),
                Some(&json!([
                    approval_meta::PERSIST_SESSION,
                    approval_meta::PERSIST_ALWAYS,
                ]))
            );
            (
                McpServerElicitationAction::Accept,
                Some(json!({approval_meta::PERSIST_KEY: approval_meta::PERSIST_ALWAYS})),
            )
        }
        ToolApprovalChoice::Decline => (McpServerElicitationAction::Decline, None),
    };
    app.send_response(
        request_id,
        serde_json::to_value(McpServerElicitationRequestResponse {
            action,
            content: None,
            meta: response_meta,
        })?,
    )
    .await?;
    Ok(())
}

async fn finish_tool_call(
    app: &mut TestAppServer,
    call_id: &str,
) -> Result<(McpToolCallStatus, Option<Value>)> {
    let result = timeout(READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if let ThreadItemPayload::McpToolCall {
                id, status, result, ..
            } = completed.item.payload
                && id == call_id
            {
                return Ok::<_, anyhow::Error>((
                    status,
                    result.map(|result| Value::Array(result.content)),
                ));
            }
        }
    })
    .await??;
    timeout(
        READ_TIMEOUT,
        app.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(result)
}
