use super::start_mcp_server;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ListMcpServerStatusParams;
use codex_app_server_protocol::ListMcpServerStatusResponse;
use codex_app_server_protocol::McpServerStatusDetail;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadMcpServerActivateOutcome;
use codex_app_server_protocol::ThreadMcpServerActivateParams;
use codex_app_server_protocol::ThreadMcpServerActivateResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(30);
const HIDDEN_SERVER: &str = "Hidden.Docs";

#[derive(Clone, Copy)]
enum ActivationMoment {
    BeforeFirstTurn,
    BetweenTurns,
}

async fn activate(
    app: &mut TestAppServer,
    thread_id: &str,
    server_name: &str,
) -> Result<ThreadMcpServerActivateResponse> {
    app.request(|request_id| ClientRequest::ThreadMcpServerActivate {
        request_id,
        params: ThreadMcpServerActivateParams {
            thread_id: thread_id.to_string(),
            server_name: server_name.to_string(),
        },
    })
    .await
}

async fn complete_turn(app: &mut TestAppServer, thread_id: &str, message: &str) -> Result<()> {
    let completed = timeout(
        TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: message.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    Ok(())
}

#[test_case(ActivationMoment::BeforeFirstTurn; "queued before initial user turn")]
#[test_case(ActivationMoment::BetweenTurns; "idle after a completed turn")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_adds_context_without_promoting_tools_or_starting_a_turn(
    moment: ActivationMoment,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let expected_requests = match moment {
        ActivationMoment::BeforeFirstTurn => 2,
        ActivationMoment::BetweenTurns => 3,
    };
    let requests = responses::mount_sse_sequence(
        &server,
        (0..expected_requests)
            .map(|index| {
                responses::sse(vec![
                    responses::ev_assistant_message(&format!("message-{index}"), "done"),
                    responses::ev_completed(&format!("response-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let (hidden_url, hidden_handle) =
        start_mcp_server("lookup_hidden", /*tools_error*/ None).await?;
    let (implicit_url, implicit_handle) =
        start_mcp_server("lookup_implicit", /*tools_error*/ None).await?;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config("model_auto_compact_token_limit = 1000000")
        .with_provider_config("supports_websockets = false")
        .disable_feature(Feature::ToolSearch)
        .disable_feature(Feature::CodeMode)
        .with_extra_config(&format!(
            r#"[mcp_servers."{HIDDEN_SERVER}"]
url = "{hidden_url}/mcp"
allow_implicit_invocation = false
[mcp_servers.implicit]
url = "{implicit_url}/mcp"
"#
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;

    if let ActivationMoment::BetweenTurns = moment {
        complete_turn(&mut app, &thread.id, "before explicit activation").await?;
        assert_eq!(
            activate(&mut app, &thread.id, "implicit").await?,
            ThreadMcpServerActivateResponse {
                outcome: ThreadMcpServerActivateOutcome::AlreadyImplicitlyAvailable,
            },
        );
    }
    assert_eq!(
        activate(&mut app, &thread.id, HIDDEN_SERVER).await?,
        ThreadMcpServerActivateResponse {
            outcome: ThreadMcpServerActivateOutcome::Activated,
        },
    );
    complete_turn(&mut app, &thread.id, "first user turn after activation").await?;
    assert_eq!(
        activate(&mut app, &thread.id, HIDDEN_SERVER).await?,
        ThreadMcpServerActivateResponse {
            outcome: ThreadMcpServerActivateOutcome::AlreadyActivated,
        },
    );
    complete_turn(&mut app, &thread.id, "second user turn after activation").await?;
    let exit = timeout(TIMEOUT, app.shutdown_gracefully()).await??;
    assert!(exit.success());

    let captured = requests.requests();
    assert_eq!(captured.len(), expected_requests);
    let first_explicit_request = expected_requests - 2;
    if let ActivationMoment::BetweenTurns = moment {
        assert!(captured[0].body_contains_text("before explicit activation"));
        assert!(
            captured[0]
                .message_input_texts("developer")
                .iter()
                .all(|text| !text.contains("<mcp_use>"))
        );
    }
    let mut inventories = Vec::new();
    for (request, message) in captured[first_explicit_request..].iter().zip([
        "first user turn after activation",
        "second user turn after activation",
    ]) {
        assert!(request.body_contains_text(message));
        let explicit = request
            .message_input_texts("developer")
            .into_iter()
            .filter(|text| text.contains("<mcp_use>"))
            .collect::<Vec<_>>();
        assert_eq!(explicit.len(), 1);
        assert!(explicit[0].contains("`Hidden.Docs`"));
        assert!(explicit[0].contains("lookup_hidden"));
        assert!(explicit[0].contains("Look up test data."));
        assert!(explicit[0].contains("additionalProperties"));
        inventories.push(explicit);
    }
    assert_eq!(inventories[0], inventories[1]);
    for request in &captured {
        assert_eq!(
            request.body_json()["tools"],
            captured[0].body_json()["tools"]
        );
        let declarations = request.body_json()["tools"].to_string();
        assert!(!declarations.contains("lookup_hidden"));
        assert!(declarations.contains("lookup_implicit"));
    }
    hidden_handle.abort();
    implicit_handle.abort();
    let _ = hidden_handle.await;
    let _ = implicit_handle.await;
    Ok(())
}

#[tokio::test]
async fn activation_rejects_unknown_disabled_and_managed_disallowed_servers() -> Result<()> {
    let server = responses::start_mock_server().await;
    let (url, handle) = start_mcp_server("lookup", /*tools_error*/ None).await?;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_extra_config(&format!(
            r#"[mcp_servers.disabled]
url = "{url}/mcp"
enabled = false
allow_implicit_invocation = false
[mcp_servers.blocked]
url = "{url}/mcp"
allow_implicit_invocation = false
"#
        ))
        .write(home.path())?;
    std::fs::write(
        home.path().join("requirements.toml"),
        "[mcp_servers.blocked.identity]\nurl = \"https://allowed.example/mcp\"\n",
    )?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let thread_id = app
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    for (server_name, message) in [
        (
            "missing",
            format!("unknown MCP server `missing` for thread {thread_id}"),
        ),
        (
            "disabled",
            format!("MCP server `disabled` is disabled for thread {thread_id}"),
        ),
        (
            "blocked",
            format!("MCP server `blocked` is disabled for thread {thread_id}"),
        ),
    ] {
        let request_id = app
            .send_raw_request(
                "thread/mcpServer/activate",
                Some(json!({"threadId": thread_id, "serverName": server_name})),
            )
            .await?;
        let error: JSONRPCError = timeout(
            TIMEOUT,
            app.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(
            error.error,
            JSONRPCErrorError {
                code: -32600,
                message,
                data: None,
            },
        );
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .all(|request| !request.url.path().ends_with("/responses"))
    );
    handle.abort();
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn status_reports_effective_implicit_policy_for_loaded_and_threadless_reads() -> Result<()> {
    let server = responses::start_mock_server().await;
    let (url, handle) = start_mcp_server("lookup", /*tools_error*/ None).await?;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_extra_config(&format!(
            r#"[mcp_servers.hidden]
url = "{url}/mcp"
allow_implicit_invocation = false
[mcp_servers.implicit]
url = "{url}/mcp"
[mcp_servers.disabled]
url = "{url}/mcp"
enabled = false
allow_implicit_invocation = false
"#
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            config: Some(HashMap::from([(
                "mcp_servers.hidden.allow_implicit_invocation".to_string(),
                json!(true),
            )])),
            ..Default::default()
        })
        .await?
        .thread;
    for (thread_id, hidden_is_implicit) in [(None, false), (Some(thread.id), true)] {
        let status: ListMcpServerStatusResponse = app
            .request(|request_id| ClientRequest::McpServerStatusList {
                request_id,
                params: ListMcpServerStatusParams {
                    cursor: None,
                    limit: None,
                    detail: Some(McpServerStatusDetail::ToolsAndAuthOnly),
                    thread_id,
                },
            })
            .await?;
        let policy = status
            .data
            .iter()
            .map(|server| (server.name.as_str(), server.allow_implicit_invocation))
            .collect::<Vec<_>>();
        assert_eq!(
            policy,
            vec![
                ("disabled", false),
                ("hidden", hidden_is_implicit),
                ("implicit", true),
            ]
        );
    }
    handle.abort();
    let _ = handle.await;
    Ok(())
}
