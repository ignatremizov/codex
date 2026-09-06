use super::*;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

#[test]
fn materialized_rows_match_ratatui_buffers() {
    for text in [
        "",
        " ",
        "abcde",
        "abcde ",
        "abcde  f",
        "  alpha   beta  ",
        "漢字 ｶﾞ 👩‍💻 e\u{301} end",
        "\u{301}A B",
        "a\u{a0}b\u{200b}c",
        "\talpha\tbeta",
    ] {
        for width in [1, 2, 5, 9, 20] {
            for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
                let line = Line::from(vec!["".green(), text.blue().bold()]).alignment(alignment);
                let paragraph = Paragraph::new(line.clone()).wrap(Wrap { trim: false });
                let rows = wrap_line_rows(&line, width, Style::default());
                let area = Rect::new(/*x*/ 0, /*y*/ 0, width, 30);
                let mut expected = Buffer::empty(area);
                let mut actual = Buffer::empty(area);
                paragraph.render(area, &mut expected);
                render_line_rows(&rows, area, &mut actual);
                assert_eq!(
                    actual, expected,
                    "text={text:?} width={width} alignment={alignment:?}"
                );
            }
        }
    }
}

#[test]
fn materialized_rows_keep_wide_graphemes_at_the_right_edge() {
    let line = Line::from("漢字 ｶﾞ 👩‍💻 e\u{301} end").blue().bold();
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 9, /*height*/ 3,
    );
    let rows = wrap_line_rows(&line, area.width, Style::default());
    let mut buffer = Buffer::empty(area);
    render_line_rows(&rows, area, &mut buffer);
    insta::assert_debug_snapshot!(buffer);
}

#[test]
fn a_single_logical_line_can_wrap_past_terminal_coordinate_limits() {
    let line = Line::from("x".repeat(65_540));
    let rows = wrap_line_rows(&line, /*width*/ 1, Style::default());
    assert_eq!(rows.len(), 65_540);
    assert_eq!(rows[65_536..], vec![Line::from("x"); 4]);
    assert!(wrap_line_rows(&line, /*width*/ 0, Style::default()).is_empty());
}
