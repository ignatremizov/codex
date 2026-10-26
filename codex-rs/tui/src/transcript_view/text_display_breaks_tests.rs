//! Statement rows remain presentation-only through source reconstruction, copy and resize.

use std::sync::Arc;

use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;

use super::TextLayout;
use super::logical::slice_line;
use crate::clipboard_copy::CopyFormat;
use crate::exec_cell::ExecCall;
use crate::exec_cell::ExecCell;
use crate::history_cell::HistoryCell;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::LineWrapPolicy;
use crate::terminal_hyperlinks::LogicalLineSource;
use crate::terminal_hyperlinks::prefix_hyperlink_lines;
use crate::terminal_hyperlinks::remap_source_wrapped_line;
use crate::wrapping::WrappedLine;
use codex_app_server_protocol::CommandExecutionSource;

/// Match the command renderer: shared source, with post-break indentation omitted from rows.
fn fragments(mut line: HyperlinkLine, breaks: Vec<usize>) -> Vec<HyperlinkLine> {
    let mut source = LogicalLineSource::from_line(&line.line);
    source.display_breaks = breaks.clone().into();
    source.wrap_policy = LineWrapPolicy::UrlAware;
    let mut boundaries = vec![0];
    boundaries.extend(breaks);
    boundaries.push(source.text.len());
    let wrapped = boundaries
        .windows(2)
        .enumerate()
        .map(|(index, pair)| {
            let mut start = pair[0];
            let end = pair[1];
            if index > 0 {
                start += source.text[start..end]
                    .bytes()
                    .take_while(|byte| matches!(*byte, b' ' | b'\t'))
                    .count();
            }
            WrappedLine {
                line: slice_line(&line.line, start..end),
                range: start..end,
                prefix_bytes: 0,
            }
        })
        .collect();
    line.source = Some(source);
    prefix_hyperlink_lines(
        remap_source_wrapped_line(&line, wrapped),
        "› ".into(),
        "  ".into(),
    )
}

fn render(layout: &TextLayout, width: u16) -> Buffer {
    let area = Rect::new(/*x*/ 0, /*y*/ 0, width, layout.row_count() as u16);
    let mut buffer = Buffer::empty(area);
    layout.render(area, &mut buffer, /*start_row*/ 0);
    buffer
}

#[test]
fn statement_fragments_copy_exact_source_and_only_real_hard_breaks() {
    let source = "echo first; \t  echo second &&  echo third";
    let breaks = vec![
        source.find(';').unwrap() + 1,
        source.find("&&").unwrap() + 2,
    ];
    let mut lines = fragments(source.into(), breaks.clone());
    let origins = lines
        .iter()
        .map(|line| line.source.as_ref().unwrap())
        .collect::<Vec<_>>();
    assert!(origins.windows(2).all(|pair| {
        Arc::ptr_eq(&pair[0].text, &pair[1].text)
            && Arc::ptr_eq(&pair[0].display_breaks, &pair[1].display_breaks)
    }));
    assert_eq!(origins[0].display_breaks.as_ref(), breaks.as_slice());
    lines.push("  actual hard line".into());

    let expected = format!("{source}\n  actual hard line");
    let layout = TextLayout::new(lines, /*width*/ 64);
    let mut selected = Vec::new();
    layout.copy_lines(0..layout.text().len(), "", &mut selected);
    assert_eq!(
        (
            layout.text(),
            layout.row_count(),
            crate::markdown_copy::selection(&selected, layout.text())
        ),
        (
            expected.as_str(),
            4,
            (expected.clone(), CopyFormat::PlainText)
        )
    );
    let start = layout.position_at(/*row*/ 0, /*column*/ 2);
    let end = layout.position_at(/*row*/ 2, /*column*/ 64);
    assert_eq!(&layout.text()[start..end], source);
}

#[test]
fn display_break_rows_survive_narrow_wide_and_restored_layout() {
    let source = "echo a; \t echo b";
    let layout = TextLayout::new(
        fragments(source.into(), vec![source.find(';').unwrap() + 1]),
        /*width*/ 12,
    );
    let expected = render(&layout, /*width*/ 12);
    let narrow = layout.rewrap(/*width*/ 7);
    let wide = narrow.rewrap(/*width*/ 64);
    let restored = wide.rewrap(/*width*/ 12);
    assert_eq!(
        (
            narrow.text(),
            wide.text(),
            restored.text(),
            narrow.row_count(),
            wide.row_count(),
            render(&restored, /*width*/ 12),
        ),
        (source, source, source, 4, 2, expected.clone())
    );
    assert_snapshot!("statement_display_breaks_rewrap", format!("{expected:?}"));
}

#[test]
fn forced_break_whitespace_is_hidden_without_becoming_a_selected_newline() {
    let source = "echo a; \t  echo b";
    let break_at = source.find(';').unwrap() + 1;
    // Even an unsplit input row must honor metadata when it is reconstructed for layout.
    let mut line = HyperlinkLine::from(source);
    let mut origin = LogicalLineSource::from_line(&line.line);
    origin.display_breaks = vec![break_at].into();
    line.source = Some(origin);
    let layout = TextLayout::new(
        prefix_hyperlink_lines(vec![line], "› ".into(), "  ".into()),
        /*width*/ 24,
    );
    let visible_start = source.rfind("echo").unwrap();
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 24, /*height*/ 2,
    );
    let mut buffer = render(&layout, /*width*/ 24);
    layout.highlight_selection(
        break_at..visible_start,
        /*next*/ None,
        area,
        &mut buffer,
        /*start_row*/ 0,
    );
    assert_eq!(
        (
            layout.text(),
            layout.row_count(),
            layout.position_at(/*row*/ 1, /*column*/ 0),
            layout.position_at(/*row*/ 1, /*column*/ 2),
            layout.first_cursor_position(/*row*/ 1),
        ),
        (source, 2, visible_start, visible_start, Some(visible_start))
    );
    assert!((break_at..visible_start).all(|offset| !layout.is_cursor_position(offset)));
    assert!(
        buffer
            .content
            .iter()
            .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
    );
}

#[test]
fn statement_rows_keep_unicode_styles_links_and_row_relative_tab_stops() {
    let prefix = "printf 界e\u{301}👩‍💻; \t  open\t";
    let destination = "https://example.com/docs";
    let mut line = HyperlinkLine::new(Line::from(prefix.green()));
    line.push_span(destination.cyan(), Some(destination));
    let break_at = prefix.find(';').unwrap() + 1;
    let layout = TextLayout::new(fragments(line, vec![break_at]), /*width*/ 40);
    let open_at = prefix.find("open").unwrap();
    let wide_at = prefix.find('界').unwrap();
    let accent_at = prefix.find('e').unwrap();
    let emoji_at = prefix.find('👩').unwrap();
    let expected = render(&layout, /*width*/ 40);
    assert_eq!(
        (
            layout.position_at(/*row*/ 0, /*column*/ 10),
            layout.position_at(/*row*/ 0, /*column*/ 11),
            layout.position_at(/*row*/ 0, /*column*/ 13),
            layout.position_at(/*row*/ 1, /*column*/ 0),
            layout.column_for_offset(prefix.len()),
            layout.link_at(/*row*/ 1, /*column*/ 8).as_deref(),
            expected[(2, 1)].fg,
            expected[(8, 1)].fg,
        ),
        (
            wide_at,
            accent_at,
            emoji_at,
            open_at,
            8,
            Some(destination),
            Color::Green,
            Color::Cyan,
        )
    );
    let narrow = layout.rewrap(/*width*/ 16);
    let link_row = narrow.row_for_offset(prefix.len());
    let link_column = narrow.column_for_offset(prefix.len());
    assert_eq!(
        (
            narrow.text(),
            narrow.link_at(link_row, link_column).as_deref(),
            narrow.position_at(link_row, link_column),
            render(&narrow.rewrap(/*width*/ 40), /*width*/ 40),
        ),
        (layout.text(), Some(destination), prefix.len(), expected,)
    );
}

#[test]
fn partial_fragments_keep_local_offsets_and_unseen_source_outside_copy() {
    let source = "echo a; \t echo b;  echo c";
    let first_break = source.find(';').unwrap() + 1;
    let second_break = source.rfind(';').unwrap() + 1;
    let lines = fragments(source.into(), vec![first_break, second_break]);
    let first = TextLayout::new(vec![lines[0].clone()], /*width*/ 24);
    let remaining = TextLayout::new(lines[1..].to_vec(), /*width*/ 24);
    let start = source.find("echo b").unwrap();
    let last = source.find("echo c").unwrap() - start;
    assert_eq!(
        (
            first.text(),
            first.separator_after(&remaining),
            remaining.text(),
            remaining.row_count(),
            remaining.position_at(/*row*/ 0, /*column*/ 2),
            remaining.position_at(/*row*/ 1, /*column*/ 2),
        ),
        ("echo a;", " \t ", &source[start..], 2, 0, last)
    );
    let mut selected = Vec::new();
    first.copy_lines(0..first.text().len(), "", &mut selected);
    remaining.copy_lines(
        0..remaining.text().len(),
        first.separator_after(&remaining),
        &mut selected,
    );
    assert_eq!(
        crate::markdown_copy::selection(&selected, source),
        (source.to_owned(), CopyFormat::PlainText)
    );
}

#[test]
fn exec_cell_review_and_transcript_keep_statement_breaks_without_changing_source() {
    let source = "printf '%s;' '界';  echo done";
    let cell = ExecCell::new(
        ExecCall {
            call_id: "statement-display-breaks".to_owned(),
            command: vec!["bash".to_owned(), "-lc".to_owned(), source.to_owned()],
            parsed: Vec::new(),
            output: None,
            source: CommandExecutionSource::UnifiedExecStartup,
            user_shell_response_handling: None,
            start_time: None,
            duration: None,
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    );
    let second_statement = source.find("echo").unwrap();
    let mut frames = Vec::new();
    for (name, lines, narrow_lines) in [
        (
            "review",
            cell.display_hyperlink_lines(/*width*/ 64),
            cell.display_hyperlink_lines(/*width*/ 18),
        ),
        (
            "transcript",
            cell.transcript_hyperlink_lines(/*width*/ 64),
            cell.transcript_hyperlink_lines(/*width*/ 18),
        ),
    ] {
        let layout = TextLayout::new(lines, /*width*/ 64);
        let narrow = layout.rewrap(/*width*/ 18);
        let fresh_narrow = TextLayout::new(narrow_lines, /*width*/ 18);
        let restored = narrow.rewrap(/*width*/ 64);
        let second_row = narrow.row_for_offset(second_statement);
        let second_column = narrow.column_for_offset(second_statement);
        let expected = render(&layout, /*width*/ 64);
        let mut selected = Vec::new();
        narrow.copy_lines(0..narrow.text().len(), "", &mut selected);
        assert_eq!(
            (
                layout.text(),
                narrow.text(),
                fresh_narrow.text(),
                restored.text(),
                layout.row_count(),
                layout.position_at(/*row*/ 1, /*column*/ 0),
                narrow.position_at(second_row, second_column),
                narrow.position_at(second_row, /*column*/ 0),
                crate::markdown_copy::selection(&selected, narrow.text()),
            ),
            (
                source,
                source,
                source,
                source,
                2,
                second_statement,
                second_statement,
                second_statement,
                (source.to_owned(), CopyFormat::PlainText),
            ),
            "{name}"
        );
        assert_eq!(
            render(&narrow, /*width*/ 18),
            render(&fresh_narrow, /*width*/ 18),
            "{name}"
        );
        assert_eq!(render(&restored, /*width*/ 64), expected, "{name}");
        frames.push(format!("{name}\n{expected:?}"));
    }
    assert_snapshot!("exec_statement_display_breaks", frames.join("\n\n"));
}
