use super::Terminal;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::HyperlinkParagraph;
use crate::test_backend::VT100Backend;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use pretty_assertions::assert_eq;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::io;

#[test]
fn fullscreen_exit_preserves_scrollback_footer_and_resume_messages() -> io::Result<()> {
    let size = Size::new(/*width*/ 40, /*height*/ 6);
    let mut terminal = Terminal::with_options(VT100Backend::with_scrollback(
        size.width,
        size.height,
        /*scrollback_len*/ 32,
    ))?;
    queue!(
        terminal.backend_mut(),
        Print("shell$ codex\r\n"),
        EnterAlternateScreen
    )?;
    terminal.set_viewport_area(Rect::from(size));
    terminal.draw(|frame| {
        Paragraph::new(vec![
            Line::from("User: keep this output"),
            Line::from("Assistant: done"),
            Line::from("Docs: 界"),
            Line::from(">"),
            Line::from("gpt-6-astra"),
            Line::from("test | ready".cyan()),
        ])
        .render(frame.area(), frame.buffer_mut());
    })?;
    let frame = terminal.previous_buffer().clone();
    queue!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.set_viewport_area(Rect::new(
        /*x*/ 0, /*y*/ 1, size.width, /*height*/ 0,
    ));
    terminal.append_frame_to_scrollback(&frame)?;
    queue!(
        terminal.backend_mut(),
        Print("To continue this session, run:\r\n  codex resume test\r\nshell$")
    )?;

    let visible = terminal.backend().vt100().screen();
    let mut scrollback = visible.clone();
    scrollback.set_scrollback(/*rows*/ usize::MAX);
    insta::assert_snapshot!(
        format!("SCROLLBACK:\n{}\nVISIBLE:\n{}", scrollback.contents(), visible.contents()),
        @r"
    SCROLLBACK:
    shell$ codex
    User: keep this output
    Assistant: done
    Docs: 界
    >
    gpt-6-astra
    VISIBLE:
    >
    gpt-6-astra
    test | ready
    To continue this session, run:
      codex resume test
    shell$
    "
    );
    assert!(!visible.alternate_screen());
    assert_eq!(
        visible.cell(/*row*/ 2, /*col*/ 0).map(vt100::Cell::fgcolor),
        Some(vt100::Color::Idx(6)),
    );
    assert_eq!(terminal.viewport_area.height, 0);
    Ok(())
}

#[test]
fn exit_replay_clips_resized_wide_cells_without_extra_wrapped_rows() -> io::Result<()> {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 8, /*height*/ 2,
    );
    let mut frame = Buffer::empty(area);
    Paragraph::new(vec![Line::from("漢字abcd"), Line::from("x")]).render(area, &mut frame);
    for (width, expected) in [(1, ""), (3, "漢"), (4, "漢字"), (8, "漢字abcd")] {
        let mut terminal = Terminal::with_options(VT100Backend::new(width, /*height*/ 4))?;
        terminal.append_frame_to_scrollback(&frame)?;
        assert_eq!(
            terminal
                .backend()
                .vt100()
                .screen()
                .rows(/*start*/ 0, width)
                .collect::<Vec<_>>(),
            [expected, "x", "", ""],
        );
        assert_eq!(
            terminal.backend().vt100().screen().cursor_position(),
            (2, 0),
        );
    }
    Ok(())
}

#[test]
fn exit_replay_keeps_hyperlinks_and_resets_style_before_shell_output() -> io::Result<()> {
    let size = Size::new(/*width*/ 12, /*height*/ 4);
    let backend = CrosstermBackend::new(Vec::<u8>::new());
    let mut terminal =
        Terminal::with_screen_size_and_cursor_position_for_test(backend, size, Position::ORIGIN);
    let mut line = HyperlinkLine::new(Line::from("docs "));
    line.push_span("漢字".cyan().underlined(), Some("https://example.com/"));
    let area = Rect::new(/*x*/ 0, /*y*/ 0, size.width, /*height*/ 1);
    let mut frame = Buffer::empty(area);
    HyperlinkParagraph::new(&[line], Style::default()).render(area, &mut frame);
    terminal.append_frame_to_scrollback(&frame)?;
    queue!(terminal.backend_mut(), Print("shell"))?;
    let bytes = terminal.backend().writer();
    let text = String::from_utf8_lossy(bytes);
    assert!(text.contains("\x1b]8;;https://example.com/\x07"));
    assert!(text.contains("\x1b]8;;\x07"));
    let mut parser = vt100::Parser::new(size.height, size.width, /*scrollback_len*/ 0);
    parser.process(bytes);
    assert_eq!(parser.screen().contents(), "docs 漢字\nshell");
    assert_eq!(
        parser.screen().cell(/*row*/ 1, /*col*/ 0).map(|cell| (
            cell.fgcolor(),
            cell.bgcolor(),
            cell.underline()
        )),
        Some((vt100::Color::Default, vt100::Color::Default, false)),
    );
    Ok(())
}
