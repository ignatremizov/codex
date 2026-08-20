//! The normal owned viewport retains code-mode details without opening a transcript or disclosure.

use super::*;
use crate::app::tests::make_test_app_with_channels;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells;
use codex_app_server_protocol::McpToolCallStatus;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test]
async fn default_owned_history_shows_complete_live_and_replayed_code_mode_calls() -> Result<()> {
    let mut snapshots = Vec::new();
    for server in ["node_repl", "cua_repl"] {
        for status in ["completed", "failed"] {
            let output = if status == "completed" {
                "Script completed\nOutput:\nfirst\nsecond\nthird\nfourth\nfinal result"
            } else {
                "Script failed\nfirst\nsecond\nthird\nfourth\nScript error:\nfinal diagnostic"
            };
            let item: ThreadItem = serde_json::from_value(json!({
                "type": "mcpToolCall",
                "id": "call",
                "server": server,
                "tool": "js",
                "status": status,
                "arguments": {"title": "Inspect", "code": "text(1)"},
                "result": {"content": [{"type": "text", "text": output}]},
                "durationMs": 1
            }))?;
            let mut rendered = Vec::new();
            for replay in [false, true] {
                let (mut app, mut events, _operations) = make_test_app_with_channels().await;
                app.local_settings.tui.animations = false;
                if replay {
                    app.transcript_cells = thread_items_to_transcript_cells(
                        /*thread_id*/ None,
                        &app.config.cwd,
                        [item.clone()],
                        RawReasoningVisibility::Hidden,
                        Some(&app.config),
                    );
                } else {
                    let mut started = item.clone();
                    if let ThreadItem::McpToolCall { status, result, .. } = &mut started {
                        *status = McpToolCallStatus::InProgress;
                        *result = None;
                    }
                    app.chat_widget.handle_mcp_tool_call_started_now(started);
                    app.chat_widget
                        .handle_mcp_tool_call_completed_now(item.clone());
                    while let Ok(event) = events.try_recv() {
                        if let AppEvent::InsertHistoryCell(cell) = event {
                            app.transcript_cells.push(Arc::from(cell));
                        }
                    }
                }
                assert!(!app.transcript_view.is_detailed());
                for cell in &app.transcript_cells {
                    assert!(!cell.has_hidden_activity_details(/*width*/ 100));
                }
                let mut tui = crate::tui::test_support::make_test_tui()?;
                tui.set_owned_screen(/*owned*/ true)?;
                let size = Size::new(/*width*/ 100, /*height*/ 24);
                tui.terminal.resize(size)?;
                let bottom = app.render_owned_transcript(&mut tui, size)?;
                let buffer =
                    crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
                let history = buffer
                    .content()
                    .chunks(usize::from(size.width))
                    .take(usize::from(bottom.y.saturating_sub(/*rhs*/ 1)))
                    .map(|row| {
                        row.iter()
                            .map(ratatui::buffer::Cell::symbol)
                            .collect::<String>()
                            .trim_end()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim_matches('\n')
                    .to_string();
                rendered.push(history);
                tui.set_owned_screen(/*owned*/ false)?;
            }
            assert_eq!(rendered[0], rendered[1]);
            snapshots.push(format!("{server} {status}\n{}", rendered[0]));
        }
    }
    insta::assert_snapshot!("default_owned_code_mode_history", snapshots.join("\n\n"));
    Ok(())
}
