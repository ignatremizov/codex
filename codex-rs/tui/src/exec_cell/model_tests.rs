use super::*;
use crate::history_cell::HistoryCell;
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
    cached.calls[0].parsed = vec![ParsedCommand::ListFiles {
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
