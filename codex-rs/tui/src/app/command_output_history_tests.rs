//! Command preview settings reach the default owned viewport in live and reconstructed history.

use super::*;
use crate::app::tests::make_test_app_with_channels;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells_with_preview_line_limits;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn owned_viewport_uses_local_tool_and_user_shell_preview_limits() -> Result<()> {
    let mut snapshots = Vec::new();
    for source in [
        CommandExecutionSource::Agent,
        CommandExecutionSource::UserShell,
    ] {
        for limits in [
            OutputPreviewLineLimits {
                command: 3,
                user_shell: 5,
            },
            OutputPreviewLineLimits {
                command: 0,
                user_shell: 0,
            },
            OutputPreviewLineLimits {
                command: 1,
                user_shell: 2,
            },
            OutputPreviewLineLimits::default(),
        ] {
            let item = ThreadItem::CommandExecution {
                id: "command".to_owned(),
                plugin_id: None,
                script_path: None,
                model_context: None,
                command: "printf output".to_owned(),
                cwd: LegacyAppPathString::from_string("/workspace"),
                process_id: None,
                source,
                user_shell_response_handling: None,
                status: CommandExecutionStatus::Completed,
                command_actions: Vec::new(),
                aggregated_output: Some(
                    (1..=8)
                        .map(|n| format!("line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                exit_code: Some(0),
                duration_ms: Some(1),
            };
            let mut histories = Vec::new();
            for replay in [false, true] {
                let (mut app, mut events, _operations) = make_test_app_with_channels().await;
                app.local_settings.tui.command_output_preview_lines = limits.command;
                app.local_settings.tui.user_shell_output_preview_lines = limits.user_shell;
                app.chat_widget.local_settings = app.local_settings.clone();
                // Server/runtime configuration is not the authority for local presentation.
                app.config.tui_command_output_preview_lines = 99;
                app.config.tui_user_shell_output_preview_lines = 99;
                if replay {
                    app.transcript_cells =
                        thread_items_to_transcript_cells_with_preview_line_limits(
                            /*thread_id*/ None,
                            &app.config.cwd,
                            [item.clone()],
                            RawReasoningVisibility::Hidden,
                            Some(&app.config),
                            limits,
                            (&app.local_settings.tui).into(),
                        );
                } else {
                    let mut started = item.clone();
                    if let ThreadItem::CommandExecution {
                        status,
                        aggregated_output,
                        ..
                    } = &mut started
                    {
                        *status = CommandExecutionStatus::InProgress;
                        *aggregated_output = None;
                    }
                    app.chat_widget
                        .handle_command_execution_started_now(started);
                    app.chat_widget
                        .handle_command_execution_completed_now(item.clone());
                    while let Ok(event) = events.try_recv() {
                        if let AppEvent::InsertHistoryCell(cell) = event {
                            app.transcript_cells.push(Arc::from(cell));
                        }
                    }
                }
                assert_eq!(app.transcript_cells.len(), 1);
                let full = app.transcript_cells[0].transcript_lines(/*width*/ 100);
                assert!(full.iter().any(|line| line.to_string() == "line 4"));
                assert!(!app.transcript_view.is_detailed());
                let mut tui = crate::tui::test_support::make_test_tui()?;
                tui.set_owned_screen(/*owned*/ true)?;
                let size = Size::new(/*width*/ 100, /*height*/ 32);
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
                            .to_owned()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim_matches('\n')
                    .to_owned();
                histories.push(history);
                tui.set_owned_screen(/*owned*/ false)?;
            }
            assert_eq!(histories[0], histories[1]);
            let limit = if source == CommandExecutionSource::UserShell {
                limits.user_shell
            } else {
                limits.command
            };
            assert_eq!(histories[0].contains("… +"), limit != 0 && limit < 8);
            if limits.command == 3 {
                snapshots.push(histories.remove(/*index*/ 0));
            }
        }
    }
    insta::assert_snapshot!(snapshots.join("\n\n"), @r"
    • Ran printf output
      └ line 1
        … +6 rows (ctrl+t to view transcript)
        line 8
        + Show details

    • You ran printf output
      └ line 1
        line 2
        … +4 rows (ctrl+t to view transcript)
        line 7
        line 8
        + Show details
    ");
    Ok(())
}
