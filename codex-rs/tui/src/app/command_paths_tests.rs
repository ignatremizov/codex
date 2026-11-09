use super::*;
use crate::exec_cell::CommandDirectory;
use crate::exec_cell::CommandDirectoryHistoryCell;
use codex_utils_path_uri::LegacyAppPathString;
use insta::assert_snapshot;

#[tokio::test]
async fn native_reflow_uses_loaded_page_order_for_directory_transitions() {
    let mut app = make_test_app().await;
    app.local_settings.tui.terminal_resize_reflow_max_rows = Some(0);
    let base = LegacyAppPathString::from_string("/workspace")
        .to_inferred_path_uri()
        .unwrap();
    // Each page is projected independently, so its initial transition refers to the thread cwd.
    app.transcript_cells = ["/workspace/service-a", "/workspace/service-a", "/workspace"]
        .into_iter()
        .map(|cwd| {
            Arc::new(CommandDirectoryHistoryCell {
                inner: history_cell::PlainHistoryCell::new(vec!["command".into()]),
                directory: CommandDirectory::new(
                    LegacyAppPathString::from_string(cwd),
                    base.clone(),
                    /*previous*/ None,
                ),
            }) as Arc<dyn HistoryCell>
        })
        .collect();
    let rendered = app
        .render_transcript_lines_for_reflow(/*width*/ 80)
        .lines
        .iter()
        .map(|line| line.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(rendered, @"
      ↳ cwd service-a
    command

    command

      ↳ cwd .
    command
    ");
}
