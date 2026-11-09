use super::*;
use crate::exec_cell::CommandDirectory;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCall;
use crate::exec_cell::ExecCell;
use codex_app_server_protocol::CommandExecutionSource;
use codex_utils_path_uri::LegacyAppPathString;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn command(cwd: &str) -> Arc<dyn HistoryCell> {
    let base = LegacyAppPathString::from_string("/workspace")
        .to_inferred_path_uri()
        .unwrap();
    Arc::new(
        ExecCell::new(
            ExecCall {
                call_id: cwd.into(),
                command: vec!["pwd".into()],
                parsed: Vec::new(),
                output: Some(CommandOutput::new(/*exit_code*/ 0, "ok\n".into())),
                source: CommandExecutionSource::Agent,
                user_shell_response_handling: None,
                start_time: None,
                duration: Some(Duration::from_millis(/*millis*/ 5)),
                interaction_input: None,
            },
            /*animations_enabled*/ false,
        )
        .with_command_directory(Some(CommandDirectory::new(
            LegacyAppPathString::from_string(cwd),
            base,
            /*previous*/ None,
        ))),
    )
}

fn prepare(view: &mut TranscriptView, cells: &[Arc<dyn HistoryCell>]) {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 20,
    );
    view.render(area, &mut Buffer::empty(area), cells);
}

#[test]
fn prepended_pages_repair_directory_context_and_the_toggle_invalidates_layouts() {
    let newer = command("/workspace");
    let mut cells = vec![newer];
    let mut view = TranscriptView::default();
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    prepare(&mut view, &cells);
    let before = view
        .current_layout(&cells, /*index*/ 0)
        .unwrap()
        .text()
        .to_string();
    assert!(!before.contains("cwd"));
    cells.insert(/*index*/ 0, command("/workspace/service-a"));
    cells.insert(/*index*/ 1, command("/workspace/service-a"));
    view.history_loaded(&cells, 0..2);
    prepare(&mut view, &cells);
    let repeated = view
        .current_layout(&cells, /*index*/ 1)
        .unwrap()
        .text()
        .to_string();
    assert!(
        !repeated.contains("cwd"),
        "same cwd across a page/turn boundary"
    );
    let relative = view
        .current_layout(&cells, /*index*/ 2)
        .unwrap()
        .text()
        .to_string();
    assert!(
        relative.contains("cwd ."),
        "return to the root must be explicit"
    );
    view.toggle_command_paths();
    prepare(&mut view, &cells);
    let full = view
        .current_layout(&cells, /*index*/ 1)
        .unwrap()
        .text()
        .to_string();
    assert!(full.contains("/workspace/service-a"));
    assert_snapshot!(format!("Relative:\n{relative}\nFull:\n{full}"), @"
    Relative:
      ↳ cwd .
    pwd
    ok
    ✓ • 5ms
    Full:
      ↳ cwd /workspace/service-a
    pwd
    ok
    ✓ • 5ms
    ");
    let bookmark = view.bookmark(&cells);
    let mut restored = TranscriptView::default();
    restored.restore_bookmark(bookmark);
    assert_eq!(
        restored.command_directory_display(),
        CommandDirectoryDisplay::Full
    );
    view.toggle_command_paths();
    prepare(&mut view, &cells);
    assert_eq!(
        view.current_layout(&cells, /*index*/ 1).unwrap().text(),
        repeated
    );
}

#[test]
fn path_toggle_preserves_reading_anchor_and_raw_commands() {
    let cells = (0..20)
        .map(|_| command("/workspace/service-a"))
        .collect::<Vec<_>>();
    let mut view = TranscriptView::default();
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    prepare(&mut view, &cells);
    view.jump_to_entry(&cells, /*index*/ 5);
    prepare(&mut view, &cells);
    let position = view.position;
    view.toggle_command_paths();
    assert_eq!(view.position, position);
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Raw);
    prepare(&mut view, &cells);
    assert_eq!(
        view.current_layout(&cells, /*index*/ 5).unwrap().text(),
        "$ pwd\nok\n✓ • 5ms"
    );
}
