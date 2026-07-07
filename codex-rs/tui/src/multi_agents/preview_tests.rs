use super::tests::agent_state;
use super::tests::cell_to_text;
use super::tests::line_to_text;
use super::tests::metadata_for;
use super::*;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[test]
fn wait_response_budgets_are_per_agent_and_leave_status_rows_visible() {
    let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("valid robie thread id");
    let bob_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("valid bob thread id");
    let cell = waiting_end(
        &[robie_id.to_string(), bob_id.to_string()],
        &HashMap::from([
            (
                robie_id.to_string(),
                agent_state(CollabAgentStatus::Completed, Some("\r\nfirst\r\nlast\r\n")),
            ),
            (
                bob_id.to_string(),
                agent_state(CollabAgentStatus::Errored, Some("first\nlast")),
            ),
        ]),
        /*agent_response_preview_lines*/ 1,
        &mut |thread_id| metadata_for(thread_id, robie_id, bob_id),
    );
    assert_snapshot!(cell_to_text(&cell), @r"
    • Finished waiting
      └ Robie [explorer]: Completed
          … +2 rows hidden
        Bob [worker]: Error
          … +2 rows hidden
    ");
    assert_eq!(
        cell.raw_lines()
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>(),
        vec![
            "• Finished waiting",
            "  └ Robie [explorer]: Completed",
            "      first",
            "      last",
            "    Bob [worker]: Error",
            "      first",
            "      last",
        ],
    );
}

#[test]
fn capped_preview_retains_complete_raw_source_and_link_metadata() {
    let source = "\r\n  first https://example.com/path\r\n\r\n  last  \r\n";
    let cell = CollabAgentHistoryCell::new(
        "title".into(),
        vec![CollabDetail::preview(
            preview_source_lines(source),
            /*max_rows*/ 2,
        )],
    );
    let lines = cell.display_hyperlink_lines(/*width*/ 80);
    assert_eq!(lines.len(), 3);
    assert!(lines[1].source.is_some());
    assert!(!lines[1].hyperlinks.is_empty());
    assert!(lines[2].source.is_none());
    assert_eq!(
        cell.raw_lines()
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>(),
        vec![
            "title",
            "  └   first https://example.com/path",
            "    ",
            "      last  "
        ],
    );
    assert_eq!(
        lines
            .iter()
            .map(|line| line_to_text(&line.line))
            .collect::<Vec<_>>(),
        vec![
            "title",
            "  └   first https://example.com/path",
            "    … +2 rows hidden"
        ],
    );
}

#[test]
fn one_row_preview_marker_never_exceeds_its_budget() {
    let cell = CollabAgentHistoryCell::new(
        "title".into(),
        vec![CollabDetail::preview(
            preview_source_lines("first\nlast"),
            /*max_rows*/ 1,
        )],
    );
    for width in [1, 4, 12, 80] {
        let lines = cell.display_lines(width);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].width() <= usize::from(width));
        let links = cell.display_hyperlink_lines(width);
        let rows = crate::terminal_hyperlinks::HyperlinkParagraph::new(
            &links[1..],
            ratatui::style::Style::default(),
        )
        .line_count(width);
        assert_eq!(rows, 1);
    }
    assert_eq!(cell.display_lines(/*width*/ 0), Vec::<Line<'static>>::new());
    assert_snapshot!(cell_to_text(&cell), @r"
    title
      └ … +2 rows hidden
    ");
}

#[test]
fn retained_preview_rows_fit_the_actual_narrow_viewport() {
    for source in ["abcdef\nlast", "界界界\nlast", "  a界b  \nlast"] {
        for width in 1..=8 {
            for max_rows in [1, 2, 3] {
                let cell = CollabAgentHistoryCell::new(
                    "title".into(),
                    vec![CollabDetail::preview(
                        preview_source_lines(source),
                        max_rows,
                    )],
                );
                let lines = cell.display_hyperlink_lines(width);
                let details = &lines[1..];
                let paragraph = crate::terminal_hyperlinks::HyperlinkParagraph::new(
                    details,
                    ratatui::style::Style::default(),
                );
                assert!(details.iter().all(
                    |line| crate::line_truncation::line_width(&line.line) <= usize::from(width)
                ));
                assert_eq!(paragraph.line_count(width), details.len());
                assert!(paragraph.line_count(width) <= max_rows);
                let title_rows = crate::terminal_hyperlinks::HyperlinkParagraph::new(
                    &lines[..1],
                    ratatui::style::Style::default(),
                )
                .line_count(width);
                assert_eq!(
                    usize::from(cell.desired_height(width)),
                    title_rows + paragraph.line_count(width)
                );
            }
        }
    }
}

#[test]
fn wait_completion_preserves_multiline_agent_response_snapshot() {
    let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
        .expect("valid sender thread id");
    let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("valid robie thread id");
    let message = "first line\n  indented line\nlast line\n";

    let item = ThreadItem::CollabAgentToolCall {
        id: "call-wait".to_string(),
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::Completed,
        sender_thread_id: sender_thread_id.to_string(),
        receiver_thread_ids: vec![robie_id.to_string()],
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::from([(
            robie_id.to_string(),
            agent_state(CollabAgentStatus::Completed, Some(message)),
        )]),
    };

    let unlimited = tool_call_history_cell(
        &item,
        /*cached_spawn_request*/ None,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
    )
    .expect("wait end item renders");
    let capped = tool_call_history_cell(
        &item,
        /*cached_spawn_request*/ None,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        /*agent_response_preview_lines*/ 2,
        |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
    )
    .expect("wait end item renders");

    let snapshot = [unlimited, capped]
        .iter()
        .map(cell_to_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_snapshot!(
        snapshot,
        @r###"
    • Finished waiting
      └ Robie [explorer]: Completed
          first line
            indented line
          last line

    • Finished waiting
      └ Robie [explorer]: Completed
          first line
          … +2 rows hidden
    "###
    );
}

#[test]
fn spawn_prompt_preview_preserves_multiline_prompt_snapshot() {
    let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
        .expect("valid sender thread id");
    let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("valid robie thread id");
    let prompt = "Review the change.\nFocus on regressions.\nDo not run tests.\nReport findings.";

    let item = ThreadItem::CollabAgentToolCall {
        id: "call-spawn".to_string(),
        tool: CollabAgentTool::SpawnAgent,
        status: CollabAgentToolCallStatus::Completed,
        sender_thread_id: sender_thread_id.to_string(),
        receiver_thread_ids: vec![robie_id.to_string()],
        prompt: Some(prompt.to_string()),
        model: Some("gpt-5".to_string()),
        reasoning_effort: Some(ReasoningEffortConfig::High),
        agents_states: HashMap::from([(
            robie_id.to_string(),
            agent_state(CollabAgentStatus::PendingInit, /*message*/ None),
        )]),
    };

    let unlimited = tool_call_history_cell(
        &item,
        /*cached_spawn_request*/ None,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
    )
    .expect("spawn item renders");
    let capped = tool_call_history_cell(
        &item,
        /*cached_spawn_request*/ None,
        /*agent_prompt_preview_lines*/ 2,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
    )
    .expect("spawn item renders");

    let snapshot = [unlimited, capped]
        .iter()
        .map(cell_to_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_snapshot!(
        snapshot,
        @r###"
    • Spawned Robie [explorer] (gpt-5 high)
      └ Review the change.
        Focus on regressions.
        Do not run tests.
        Report findings.

    • Spawned Robie [explorer] (gpt-5 high)
      └ Review the change.
        … +3 rows hidden
    "###
    );
}

#[test]
fn preview_caps_wrapped_rows_for_long_single_lines() {
    let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
        .expect("valid sender thread id");
    let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("valid robie thread id");
    let long_text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";

    let spawn = tool_call_history_cell(
        &ThreadItem::CollabAgentToolCall {
            id: "call-spawn".to_string(),
            tool: CollabAgentTool::SpawnAgent,
            status: CollabAgentToolCallStatus::Completed,
            sender_thread_id: sender_thread_id.to_string(),
            receiver_thread_ids: vec![robie_id.to_string()],
            prompt: Some(long_text.to_string()),
            model: Some("gpt-5".to_string()),
            reasoning_effort: Some(ReasoningEffortConfig::High),
            agents_states: HashMap::from([(
                robie_id.to_string(),
                agent_state(CollabAgentStatus::PendingInit, /*message*/ None),
            )]),
        },
        /*cached_spawn_request*/ None,
        /*agent_prompt_preview_lines*/ 2,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
    )
    .expect("spawn item renders");

    let wait = tool_call_history_cell(
        &ThreadItem::CollabAgentToolCall {
            id: "call-wait".to_string(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::Completed,
            sender_thread_id: sender_thread_id.to_string(),
            receiver_thread_ids: vec![robie_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::from([(
                robie_id.to_string(),
                agent_state(CollabAgentStatus::Completed, Some(long_text)),
            )]),
        },
        /*cached_spawn_request*/ None,
        UNLIMITED_AGENT_PREVIEW_ROWS,
        /*agent_response_preview_lines*/ 2,
        |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
    )
    .expect("wait item renders");

    let spawn_lines = spawn.display_lines(/*width*/ 28);
    let spawn_prompt_rows = &spawn_lines[1..];
    assert_eq!(spawn_prompt_rows.len(), 2);
    assert!(
        line_to_text(
            spawn_prompt_rows
                .last()
                .expect("hidden marker should render")
        )
        .contains("rows hidden")
    );

    let wait_lines = wait.display_lines(/*width*/ 28);
    let response_preview_rows = wait_lines
        .iter()
        .filter(|line| {
            let text = line_to_text(line);
            text.contains("alpha") || text.contains("rows hidden")
        })
        .count();
    assert_eq!(response_preview_rows, 2);
    let hidden_marker_index = wait_lines
        .iter()
        .position(|line| line_to_text(line).contains("rows hidden"))
        .expect("hidden marker should render");
    assert_eq!(wait_lines.len() - hidden_marker_index, 1);
    assert!(hidden_marker_index >= 2);
}
