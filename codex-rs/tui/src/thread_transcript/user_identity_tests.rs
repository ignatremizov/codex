use super::*;
use codex_app_server_protocol::UserInput;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;

#[test]
fn projected_prompt_retains_canonical_identity_and_matches_live_context_projection() {
    let cwd = test_path_buf("/tmp").abs();
    let items = [ThreadItem::UserMessage {
        id: "user-2".into(),
        client_id: Some("submission-2".into()),
        content: vec![UserInput::Text {
            text: "# Context from my IDE setup:\n\n## Active file: src/lib.rs\n\n## My request for Codex:\nInspect the change".into(),
            text_elements: Vec::new(),
        }],
    }];
    let cells = thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &cwd,
        items.clone(),
        RawReasoningVisibility::Hidden,
        /*config*/ None,
    );
    attach_projected_user_identities(&cells, items.iter().map(|item| (Some("turn-2"), item)));
    let user = cells[0]
        .as_any()
        .downcast_ref::<UserHistoryCell>()
        .expect("prompt");
    assert_eq!(
        (
            user.identity.get(),
            user.client_id.as_deref(),
            user.message.as_str()
        ),
        (
            Some(&UserMessageIdentity {
                turn_id: "turn-2".into(),
                item_id: "user-2".into()
            }),
            Some("submission-2"),
            "Inspect the change"
        ),
    );
    let rendered = cells
        .iter()
        .flat_map(|cell| cell.transcript_lines(/*width*/ 80))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"
    › Inspect the change

    ");
}
