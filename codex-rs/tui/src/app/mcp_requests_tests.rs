use super::*;
use crate::app::test_support::make_test_app;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use pretty_assertions::assert_eq;

#[test]
fn inventory_sequences_are_per_thread_and_survive_reset() {
    let mut requests = McpRequests::default();
    let first = Some(ThreadId::new());
    let second = Some(ThreadId::new());
    let stale = requests.start(first);
    let other = requests.start(second);
    let current = requests.start(first);
    assert_eq!(
        (
            requests.is_current(first, stale),
            requests.is_current(first, current),
            requests.is_current(second, other),
        ),
        (false, true, true)
    );
    requests.reset();
    let next = requests.start(first);
    assert_eq!(
        (
            requests.is_current(first, current),
            requests.is_current(first, next)
        ),
        (false, true)
    );
    assert!(next > current);
}

#[tokio::test]
async fn offscreen_activation_is_buffered_and_only_rendered_on_replay() {
    let mut app = make_test_app().await;
    let (chat, sender, mut receiver, _ops) = make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat;
    app.app_event_tx = sender;
    let origin = ThreadId::new();
    app.active_thread_id = Some(ThreadId::new());
    app.enqueue_mcp_result(
        Some(origin),
        McpThreadEvent::Activation {
            server_name: "team docs".into(),
            result: Ok(ThreadMcpServerActivateOutcome::Activated),
        },
    )
    .await;
    assert!(receiver.try_recv().is_err());
    let store = Arc::clone(&app.ensure_thread_channel(origin).store);
    let event = store
        .lock()
        .await
        .buffer
        .front()
        .cloned()
        .expect("buffered activation");
    app.active_thread_id = Some(origin);
    app.handle_thread_event_replay(event, ReplayKind::ThreadSnapshot);
    let mut rendered = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            rendered.extend(
                cell.display_lines(/*width*/ 100)
                    .iter()
                    .map(ToString::to_string),
            );
        }
    }
    insta::assert_snapshot!(rendered.join("\n"), @"• MCP server `team docs` activation requested.");
}

#[tokio::test]
async fn activation_outcomes_distinguish_admission_existing_visibility_and_failure() {
    let mut app = make_test_app().await;
    let (chat, sender, mut receiver, _ops) = make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat;
    app.app_event_tx = sender;
    for result in [
        Ok(ThreadMcpServerActivateOutcome::Activated),
        Ok(ThreadMcpServerActivateOutcome::AlreadyActivated),
        Ok(ThreadMcpServerActivateOutcome::AlreadyImplicitlyAvailable),
        Err("server is disabled".to_string()),
    ] {
        app.render_mcp_result(McpThreadEvent::Activation {
            server_name: "docs".into(),
            result,
        });
    }
    let mut rendered = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            rendered.extend(
                cell.display_lines(/*width*/ 120)
                    .iter()
                    .map(ToString::to_string),
            );
        }
    }
    insta::assert_snapshot!(rendered.join("\n"), @r"
    • MCP server `docs` activation requested.
    • MCP server `docs` tools are already in context.
    • MCP server `docs` is already visible to the model by default.
    ■ Failed to use MCP server `docs`: server is disabled
    ");
}

#[tokio::test]
async fn activation_failure_preserves_rpc_details_through_routing_and_replay() -> Result<()> {
    let home = tempfile::tempdir()?;
    let loader = LoaderOverrides::without_managed_config_for_tests();
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(home.path().to_path_buf()),
            ..Default::default()
        })
        .loader_overrides(loader.clone())
        .build()
        .await?;
    let client = Box::pin(crate::start_embedded_app_server(
        Default::default(),
        config.clone(),
        Vec::new(),
        loader,
        /*strict_config*/ false,
        Default::default(),
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        /*state_db*/ None,
        Arc::new(EnvironmentManager::default_for_tests()),
        Default::default(),
    ))
    .await?;
    let mut server = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        codex_app_server_client::AppServerClient::InProcess(client),
        crate::app_server_session::ThreadParamsMode::Embedded,
    );
    let thread_id = server.start_thread(&config).await?.session.thread_id;
    let mut app = make_test_app().await;
    let (chat, sender, mut receiver, _ops) = make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat;
    app.app_event_tx = sender;
    app.active_thread_id = Some(thread_id);
    assert!(
        app.try_submit_active_thread_op_via_app_server(
            &mut server,
            thread_id,
            &AppCommand::ActivateMcpServer {
                server_name: "node_repl".to_string(),
            },
        )
        .await?
    );
    let store = Arc::clone(&app.ensure_thread_channel(thread_id).store);
    let event = store
        .lock()
        .await
        .buffer
        .front()
        .cloned()
        .ok_or_else(|| color_eyre::eyre::eyre!("activation result was not buffered"))?;
    app.handle_thread_event_replay(event, ReplayKind::ThreadSnapshot);
    let mut rendered = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            rendered.extend(
                cell.display_lines(/*width*/ 220)
                    .iter()
                    .map(ToString::to_string),
            );
        }
    }
    insta::assert_snapshot!(
        rendered.join("\n").replace(&thread_id.to_string(), "<thread>"),
        @"■ Failed to use MCP server `node_repl`: thread/mcpServer/activate failed: thread/mcpServer/activate failed: unknown MCP server `node_repl` for thread <thread> (code -32600)"
    );
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn inventory_results_drop_superseded_requests_before_buffering() {
    let mut app = make_test_app().await;
    let origin = ThreadId::new();
    let stale = app.mcp_requests.start(Some(origin));
    let current = app.mcp_requests.start(Some(origin));
    app.handle_mcp_inventory_result(
        Ok(Vec::new()),
        McpServerStatusDetail::ToolsAndAuthOnly,
        Some(origin),
        stale,
    )
    .await;
    assert!(!app.thread_event_channels.contains_key(&origin));
    app.handle_mcp_inventory_result(
        Ok(Vec::new()),
        McpServerStatusDetail::ToolsAndAuthOnly,
        Some(origin),
        current,
    )
    .await;
    let store = Arc::clone(&app.ensure_thread_channel(origin).store);
    assert_eq!(store.lock().await.buffer.len(), 1);
    app.mcp_requests.reset();
    app.handle_mcp_inventory_result(
        Ok(Vec::new()),
        McpServerStatusDetail::ToolsAndAuthOnly,
        Some(origin),
        current,
    )
    .await;
    assert_eq!(store.lock().await.buffer.len(), 1);
}
