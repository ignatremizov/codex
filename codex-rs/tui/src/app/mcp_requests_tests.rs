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
    app.handle_thread_event_replay(event);
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
