//! Patch expansion defaults remain separate from explicit per-activity disclosure choices.

use super::*;
use crate::diff_model::FileChange;
use crate::exec_cell::ActiveExecCall;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::OutputPreviewLineLimits;
use crate::exec_cell::new_active_exec_command;
use crate::history_cell::new_patch_event;
use crate::transcript_view::tests::render;
use crate::transcript_view::tests::text;
use codex_app_server_protocol::CommandExecutionSource;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

fn patch(id: &str) -> Arc<dyn HistoryCell> {
    Arc::new(
        new_patch_event(
            HashMap::from([(
                PathBuf::from("new.txt"),
                FileChange::Add {
                    content: "one\ntwo\nthree\nfour\nfive\n".into(),
                },
            )]),
            Path::new("/project"),
        )
        .with_activity_id(id.to_owned()),
    )
}

#[test]
fn patch_expansion_default_refreshes_cached_previews_and_preserves_command_previews() {
    let cells = [patch("current")];
    let mut view = TranscriptView::default();
    let compact = text(&render(
        &mut view, &cells, /*width*/ 40, /*height*/ 10,
    ))
    .trim_end()
    .to_owned();
    view.expand_patch_edits = true;
    let expanded = text(&render(
        &mut view, &cells, /*width*/ 40, /*height*/ 10,
    ))
    .trim_end()
    .to_owned();
    insta::assert_snapshot!(
        format!("Default\n{compact}\n\nExpanded\n{expanded}"),
        @"
        Default
        • Added new.txt (+5 -0)
            1 +one
            2 +two
            3 +three
            + Show details

        Expanded
        • Added new.txt (+5 -0)
            1 +one
            2 +two
            3 +three
            4 +four
            5 +five
            − Show less
        "
    );
    view.expand_patch_edits = false;
    assert_eq!(
        text(&render(
            &mut view, &cells, /*width*/ 40, /*height*/ 10,
        ))
        .trim_end(),
        compact,
    );

    let mut command = new_active_exec_command(
        ActiveExecCall {
            call_id: "command".into(),
            command: vec!["echo".into()],
            parsed: Vec::new(),
            source: CommandExecutionSource::Agent,
            user_shell_response_handling: None,
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    )
    .with_output_preview_line_limits(OutputPreviewLineLimits {
        command: 3,
        user_shell: 3,
    });
    command.complete_call(
        "command",
        CommandOutput::new(/*exit_code*/ 0, "one\ntwo\nthree\nfour\nfive\n".into()),
        std::time::Duration::ZERO,
    );
    let cells: [Arc<dyn HistoryCell>; 1] = [Arc::new(command)];
    let compact = render(&mut view, &cells, /*width*/ 40, /*height*/ 10);
    view.expand_patch_edits = true;
    assert_eq!(
        render(&mut view, &cells, /*width*/ 40, /*height*/ 10,),
        compact,
    );
}

#[test]
fn manual_patch_disclosure_survives_history_loading_and_preference_changes() {
    let mut cells = vec![patch("current")];
    let mut view = TranscriptView {
        expand_patch_edits: true,
        ..Default::default()
    };
    let expanded = text(&render(
        &mut view, &cells, /*width*/ 40, /*height*/ 20,
    ));
    view.handle_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE), &cells);
    view.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &cells);
    assert_eq!(
        text(&render(
            &mut view, &cells, /*width*/ 40, /*height*/ 20,
        )),
        expanded,
    );
    view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &cells);
    let collapsed = render(&mut view, &cells, /*width*/ 40, /*height*/ 20);
    // Reloading the default does not erase an explicit collapse.
    for enabled in [false, true] {
        view.expand_patch_edits = enabled;
        assert_eq!(
            render(&mut view, &cells, /*width*/ 40, /*height*/ 20,),
            collapsed,
        );
    }
    // Reconstructed history retains activity identity, even after an older page is prepended.
    cells[0] = patch("current");
    cells.insert(/*index*/ 0, patch("older"));
    view.jump_to_entry(&cells, /*index*/ 0);
    let loaded = text(&render(
        &mut view, &cells, /*width*/ 40, /*height*/ 20,
    ))
    .trim_end()
    .to_owned();
    view.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &cells);
    view.expand_patch_edits = false;
    let explicit = text(&render(
        &mut view, &cells, /*width*/ 40, /*height*/ 20,
    ))
    .trim_end()
    .to_owned();
    insta::assert_snapshot!(
        format!("Loaded history\n{loaded}\n\nExplicit expansion with default off\n{explicit}"),
        @"
        Loaded history
        • Added new.txt (+5 -0)
            1 +one
            2 +two
            3 +three
            4 +four
            5 +five
            − Show less

        • Added new.txt (+5 -0)
            1 +one
            2 +two
            3 +three
            + Show details

        Explicit expansion with default off
        • Added new.txt (+5 -0)
            1 +one
            2 +two
            3 +three
            + Show details

        • Added new.txt (+5 -0)
            1 +one
            2 +two
            3 +three
            4 +four
            5 +five
            − Show less
        "
    );
}
