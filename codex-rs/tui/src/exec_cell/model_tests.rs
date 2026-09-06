use super::*;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::terminal_hyperlinks::lines_with_sources_eq;
use pretty_assertions::assert_eq;

fn cell() -> ExecCell {
    ExecCell::new(
        ExecCall {
            call_id: "call".into(),
            command: vec![
                "bash".into(),
                "-lc".into(),
                "printf 'a long command for wrapping'".into(),
            ],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            user_shell_response_handling: None,
            start_time: None,
            duration: None,
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    )
}

fn settled_exploration(id: &str) -> ExecCell {
    let mut cell = cell();
    cell.group.calls[0].call_id = id.into();
    cell.group.calls[0].command = vec!["ls".into()];
    cell.group.calls[0].parsed = vec![ParsedCommand::ListFiles {
        cmd: "ls".into(),
        path: None,
    }];
    assert!(cell.complete_call(
        id,
        CommandOutput::new(/*exit_code*/ 0, format!("{id} output\n")),
        Duration::from_secs(/*secs*/ 1),
    ));
    cell
}

#[test]
fn late_details_invalidate_settled_review_and_full_layouts() {
    for through_trait in [false, true] {
        let mut cached = settled_exploration("call");
        let before = cached.transcript_hyperlink_lines(/*width*/ 80);
        let _ = cached.display_hyperlink_lines(/*width*/ 80);
        let detail = || PlainHistoryCell::new(vec!["late transcript detail".into()]);
        if through_trait {
            assert!(cached.append_reasoning(Box::new(detail())).is_ok());
        } else {
            cached.push_detail(Arc::new(detail()));
        }
        let mut fresh = settled_exploration("call");
        fresh.push_detail(Arc::new(detail()));
        let after = cached.transcript_hyperlink_lines(/*width*/ 80);
        assert!(!lines_with_sources_eq(&before, &after));
        assert!(lines_with_sources_eq(
            &after,
            &fresh.transcript_hyperlink_lines(/*width*/ 80),
        ));
        assert!(lines_with_sources_eq(
            &cached.display_hyperlink_lines(/*width*/ 80),
            &fresh.display_hyperlink_lines(/*width*/ 80),
        ));
    }
}

#[test]
fn settled_group_append_and_prepend_keep_calls_and_details_in_cached_layouts() {
    for prepend in [false, true] {
        let mut cached = settled_exploration("middle");
        cached.push_detail(Arc::new(PlainHistoryCell::new(vec![
            "middle detail".into(),
        ])));
        let before = cached.transcript_hyperlink_lines(/*width*/ 40);
        let _ = cached.display_hyperlink_lines(/*width*/ 40);
        let mut added = settled_exploration("added");
        added.push_detail(Arc::new(PlainHistoryCell::new(vec!["added detail".into()])));
        if prepend {
            cached.prepend(added);
        } else {
            assert!(cached.append_completed(added).is_ok());
        }
        let rendered = cached.transcript_hyperlink_lines(/*width*/ 40);
        assert!(!lines_with_sources_eq(&before, &rendered));
        let text = cached
            .raw_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("middle detail") && text.contains("added detail"));
        let calls = cached
            .iter_calls()
            .map(|call| call.call_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            calls,
            if prepend {
                vec!["added", "middle"]
            } else {
                vec!["middle", "added"]
            }
        );
        assert!(lines_with_sources_eq(
            &rendered,
            &cached.transcript_hyperlink_lines(/*width*/ 40),
        ));
    }
}

#[test]
fn finalized_line_counts_are_initialized_once_and_live_counts_stay_current() {
    let mut output = CommandOutput::new(/*exit_code*/ 0, "\nfirst\nlast\n".into());
    assert_eq!(output.finalized_line_count.get(), None);
    assert_eq!(output.line_counts(), (3, 3));
    let count = output.finalized_line_count.get().unwrap() as *const usize;
    assert_eq!(output.line_counts(), (3, 3));
    assert_eq!(
        output.finalized_line_count.get().unwrap() as *const usize,
        count
    );

    let live = output
        .live_output
        .get_or_insert_with(LiveCommandOutput::default);
    live.push_str("one\n");
    assert_eq!(output.line_counts(), (1, 1));
    output.live_output.as_mut().unwrap().push_str("two\n");
    assert_eq!(output.line_counts(), (2, 2));
}

#[test]
fn live_updates_completion_replacement_and_resize_match_fresh_rendering() {
    let mut cached = cell();
    assert!(cached.append_output("call", "first\n"));
    let first = cached.display_lines(/*width*/ 80);
    assert!(cached.append_output("call", "second\n"));
    assert_ne!(cached.display_lines(/*width*/ 80), first);

    for output in [
        "\u{1b}[31mred\u{1b}[0m\nhttps://example.com/a/long/link\n",
        "replacement\n",
    ] {
        assert!(cached.complete_call(
            "call",
            CommandOutput::new(/*exit_code*/ 0, output.into()),
            Duration::from_secs(1),
        ));
        let mut fresh = cell();
        assert!(fresh.complete_call(
            "call",
            CommandOutput::new(/*exit_code*/ 0, output.into()),
            Duration::from_secs(1),
        ));
        for width in [80, 80, 20, 20, 80] {
            assert_eq!(cached.display_lines(width), fresh.display_lines(width));
            assert_eq!(
                cached.transcript_lines(width),
                fresh.transcript_lines(width)
            );
        }
    }
}

#[test]
fn extending_a_settled_group_and_failing_it_invalidates_cached_layouts() {
    let mut cached = cell();
    cached.group.calls[0].parsed = vec![ParsedCommand::ListFiles {
        cmd: "ls".into(),
        path: None,
    }];
    assert!(cached.complete_call(
        "call",
        CommandOutput::new(/*exit_code*/ 0, "first\n".into()),
        Duration::from_secs(1),
    ));
    let settled = cached.display_lines(/*width*/ 80);
    let full = cached.transcript_lines(/*width*/ 80);
    assert!(cached.add_call(
        "second".into(),
        vec!["ls".into()],
        vec![ParsedCommand::ListFiles {
            cmd: "ls".into(),
            path: None,
        }],
        ExecCommandSource::Agent,
        /*user_shell_response_handling*/ None,
        /*interaction_input*/ None,
    ));
    assert_ne!(cached.display_lines(/*width*/ 80), settled);
    assert_ne!(cached.transcript_lines(/*width*/ 80), full);
    assert!(cached.append_output("second", "second output\n"));
    cached.mark_failed();
    let failed = cached.transcript_lines(/*width*/ 80);
    assert_eq!(cached.transcript_lines(/*width*/ 80), failed);
    assert!(cached.append_output("second", "late output\n"));
    assert_ne!(cached.transcript_lines(/*width*/ 80), failed);
}

#[test]
fn preview_limit_changes_invalidate_review_without_losing_full_output() {
    let mut cached = cell();
    assert!(cached.complete_call(
        "call",
        CommandOutput::new(
            /*exit_code*/ 0,
            "one\ntwo\nthree\nfour\nfive\nsix\n".into()
        ),
        Duration::from_secs(1),
    ));
    let full = cached.transcript_lines(/*width*/ 80);
    let review = cached.display_lines(/*width*/ 80);
    let cached = cached.with_output_preview_line_limits(OutputPreviewLineLimits {
        command: 1,
        user_shell: 1,
    });
    assert_ne!(cached.display_lines(/*width*/ 80), review);
    assert_eq!(cached.transcript_lines(/*width*/ 80), full);
}
