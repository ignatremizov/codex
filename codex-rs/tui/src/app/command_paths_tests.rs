use super::*;
use crate::exec_cell::CommandDirectory;
use crate::exec_cell::CommandDirectoryHistoryCell;
use codex_utils_path_uri::LegacyAppPathString;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

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
                inner: history_cell::PlainHistoryCell::new(vec!["command".into()])
                    .with_tool_label("command".to_string()),
                directory: CommandDirectory::new(
                    LegacyAppPathString::from_string(cwd),
                    base.clone(),
                    /*previous*/ None,
                ),
            }) as Arc<dyn HistoryCell>
        })
        .collect();
    assert_eq!(
        app.transcript_cells
            .iter()
            .map(|cell| cell.tool_call_labels())
            .collect::<Vec<_>>(),
        vec![vec!["command".to_string()]; 3],
    );
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
