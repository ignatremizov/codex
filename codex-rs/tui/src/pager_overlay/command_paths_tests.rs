use super::*;
use crate::exec_cell::ActiveExecCall;
use crate::exec_cell::CommandDirectory;
use crate::exec_cell::CommandDirectoryDisplay;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::new_active_exec_command;
use crate::keymap::RuntimeKeymap;
use codex_app_server_protocol::CommandExecutionSource;
use codex_utils_path_uri::LegacyAppPathString;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn command() -> Arc<dyn HistoryCell> {
    let base = LegacyAppPathString::from_string("/workspace")
        .to_inferred_path_uri()
        .unwrap();
    let mut cell = new_active_exec_command(
        ActiveExecCall {
            call_id: "command".into(),
            command: vec!["pwd".into()],
            parsed: Vec::new(),
            source: CommandExecutionSource::Agent,
            user_shell_response_handling: None,
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    )
    .with_command_directory(Some(CommandDirectory::new(
        LegacyAppPathString::from_string("/workspace/service-a"),
        base,
        /*previous*/ None,
    )));
    assert!(cell.complete_call(
        "command",
        CommandOutput::new(/*exit_code*/ 0, "ok\n".into()),
        Duration::from_millis(/*millis*/ 5),
    ));
    Arc::new(cell)
}

fn paint(overlay: &mut TranscriptOverlay) -> String {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 100, /*height*/ 10,
    );
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    buffer
        .content()
        .chunks(usize::from(area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn p_toggles_paths_in_review_and_full_without_claiming_find_input() {
    for detailed in [false, true] {
        let cell = command();
        let mut overlay = TranscriptOverlay::new(vec![cell], RuntimeKeymap::defaults().pager);
        overlay
            .view
            .open_review_browser(crate::history_cell::HistoryRenderMode::Rich);
        // Browser keys defer until a viewport has been painted.
        paint(&mut overlay);
        if detailed {
            overlay.handle_key(KeyCode::Char('v').into());
        }
        assert_eq!(overlay.is_detailed(), detailed);
        let short = paint(&mut overlay);
        assert!(short.contains("cwd service-a"));
        assert!(short.contains("p full paths"));
        let toggle = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(overlay.owns_interaction_key(toggle));
        overlay.handle_key(toggle);
        let full = paint(&mut overlay);
        assert!(full.contains("cwd /workspace/service-a"));
        assert!(full.contains("p short paths"));
        if detailed {
            let body = |screen: &str| {
                screen
                    .lines()
                    .skip(1)
                    .take(4)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert_snapshot!(format!("Short:\n{}\nFull:\n{}", body(&short), body(&full)), @"
            Short:
              ↳ cwd service-a
            $ pwd
            ok
            ✓ • 5ms
            Full:
              ↳ cwd /workspace/service-a
            $ pwd
            ok
            ✓ • 5ms
            ");
        }
        overlay.handle_key(KeyEvent {
            kind: KeyEventKind::Release,
            ..toggle
        });
        assert_eq!(
            overlay.command_directory_display(),
            CommandDirectoryDisplay::Full
        );
        overlay.handle_key(toggle);
        assert_eq!(paint(&mut overlay), short);
        overlay.begin_search();
        overlay.handle_key(toggle);
        assert_eq!(
            overlay.command_directory_display(),
            CommandDirectoryDisplay::Changes
        );
        assert!(
            paint(&mut overlay).contains("p"),
            "p remains ordinary query input"
        );
    }
}

#[test]
fn changing_paths_refreshes_a_cached_live_command() {
    let cell = command();
    let mut overlay = TranscriptOverlay::new(Vec::new(), RuntimeKeymap::defaults().pager);
    let key = Some(ActiveCellTranscriptKey {
        cacheable: true,
        revision: 1,
        is_stream_continuation: false,
        animation_tick: None,
    });
    let sync = |overlay: &mut TranscriptOverlay| {
        let display = overlay.command_directory_display();
        overlay.sync_live_tail(/*width*/ 100, key, |width| {
            Some(cell.command_directory().unwrap().replace_lines(
                width,
                display,
                cell.transcript_hyperlink_lines(width),
            ))
        })
    };
    assert!(sync(&mut overlay));
    assert!(paint(&mut overlay).contains("cwd service-a"));
    assert!(!sync(&mut overlay));
    overlay.handle_key(KeyCode::Char('p').into());
    assert!(sync(&mut overlay));
    assert!(paint(&mut overlay).contains("cwd /workspace/service-a"));
}
