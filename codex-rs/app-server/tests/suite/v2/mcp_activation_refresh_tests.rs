use super::*;
use codex_app_server_protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;
use test_case::test_case;

async fn activation_error(app: &mut TestAppServer, thread_id: &str) -> Result<JSONRPCErrorError> {
    let request_id = app
        .send_raw_request(
            "thread/mcpServer/activate",
            Some(json!({"threadId": thread_id, "serverName": "node_repl"})),
        )
        .await?;
    let error = timeout(
        TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    Ok(error.error)
}

async fn selected_status(
    app: &mut TestAppServer,
    thread_id: &str,
) -> Result<ListMcpServerStatusResponse> {
    app.request(|request_id| ClientRequest::McpServerStatusList {
        request_id,
        params: ListMcpServerStatusParams {
            server_name: Some("node_repl".to_string()),
            thread_id: Some(thread_id.to_string()),
            detail: Some(McpServerStatusDetail::ToolsAndAuthOnly),
            cursor: None,
            limit: None,
        },
    })
    .await
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_loads_new_server_only_into_the_requesting_thread(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_sequence(
        &server,
        (0..3)
            .map(|index| responses::sse_completed(&format!("response-{index}")))
            .collect(),
    )
    .await;
    let (url, handle) = start_mcp_server("run_javascript", /*tools_error*/ None).await?;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_provider_config("supports_websockets = false")
        .disable_feature(Feature::ToolSearch)
        .disable_feature(Feature::CodeMode)
        // Keep resource helpers present before the new server is registered, so the
        // declaration comparison isolates the frozen direct MCP tool contract.
        .with_extra_config(&format!(
            "[mcp_servers.seed]\nurl = \"{url}/mcp\"\nallow_implicit_invocation = false\n"
        ))
        .write(home.path())?;
    let config_path = home.path().join("config.toml");
    let initial = std::fs::read_to_string(&config_path)?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let target = app
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread;
    let other = app.start_thread(ThreadStartParams::default()).await?.thread;
    for thread_id in [&target.id, &other.id] {
        complete_turn(&mut app, thread_id, "freeze the initial tool contract").await?;
    }
    let updated = format!(
        "{initial}\n[mcp_servers.node_repl]\nurl = \"{url}/mcp\"\nallow_implicit_invocation = false\n"
    );
    std::fs::write(&config_path, &updated)?;
    // Inventory already sees the new disk configuration, while both thread runtimes are stale.
    let inventory: ListMcpServerStatusResponse = app
        .request(|request_id| ClientRequest::McpServerStatusList {
            request_id,
            params: ListMcpServerStatusParams {
                thread_id: Some(target.id.clone()),
                detail: Some(McpServerStatusDetail::ToolsAndAuthOnly),
                server_name: None,
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(
        inventory
            .data
            .iter()
            .any(|server| server.name == "node_repl")
    );
    for thread_id in [&target.id, &other.id] {
        assert!(selected_status(&mut app, thread_id).await?.data.is_empty());
    }
    assert_eq!(
        ThreadMcpServerActivateResponse {
            outcome: ThreadMcpServerActivateOutcome::Activated,
        },
        activate(&mut app, &target.id, "node_repl").await?
    );
    let status = selected_status(&mut app, &target.id).await?;
    assert_eq!(
        vec!["run_javascript"],
        status
            .data
            .iter()
            .flat_map(|server| server.tools.values().map(|tool| tool.name.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(selected_status(&mut app, &other.id).await?.data.is_empty());
    assert_eq!(
        2,
        model.requests().len(),
        "activation must not start inference"
    );
    complete_turn(&mut app, &target.id, "use the newly activated inventory").await?;
    let captured = model.requests();
    assert_eq!(3, captured.len());
    let blocks = captured[2]
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.contains("<mcp_use>"))
        .collect::<Vec<_>>();
    assert_eq!(1, blocks.len());
    assert!(blocks[0].contains("node_repl"));
    assert!(blocks[0].contains("run_javascript"));
    assert_eq!(
        captured[0].body_json()["tools"],
        captured[2].body_json()["tools"]
    );

    // A later revocation is applied by activation itself, without a global reload.
    std::fs::write(&config_path, format!("{updated}enabled = false\n"))?;
    assert_eq!(
        activation_error(&mut app, &target.id).await?,
        JSONRPCErrorError {
            code: -32600,
            message: format!(
                "MCP server `node_repl` is disabled for thread {}",
                target.id
            ),
            data: None,
        }
    );
    assert!(selected_status(&mut app, &other.id).await?.data.is_empty());
    let exit = timeout(TIMEOUT, app.shutdown_gracefully()).await??;
    assert!(exit.success());
    handle.abort();
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn activation_refresh_preserves_session_mcp_overrides_and_reports_load_failures() -> Result<()>
{
    let server = responses::start_mock_server().await;
    let (url, handle) = start_mcp_server("run_javascript", /*tools_error*/ None).await?;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_extra_config(&format!("[mcp_servers.node_repl]\nurl = \"{url}/mcp\"\n"))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            config: Some(HashMap::from([(
                "mcp_servers.node_repl.enabled".to_string(),
                json!(false),
            )])),
            ..Default::default()
        })
        .await?
        .thread;
    assert_eq!(
        activation_error(&mut app, &thread.id).await?,
        JSONRPCErrorError {
            code: -32600,
            message: format!(
                "MCP server `node_repl` is disabled for thread {}",
                thread.id
            ),
            data: None,
        }
    );
    std::fs::write(home.path().join("config.toml"), "[mcp_servers.\n")?;
    let malformed = activation_error(&mut app, &thread.id).await?;
    assert_eq!(malformed.code, -32603);
    assert!(
        malformed
            .message
            .contains("failed to refresh MCP configuration")
    );
    let exit = timeout(TIMEOUT, app.shutdown_gracefully()).await??;
    assert!(exit.success());
    handle.abort();
    let _ = handle.await;
    Ok(())
}
