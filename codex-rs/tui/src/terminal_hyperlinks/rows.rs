//! Cached static text rows, using Ratatui's trim:false packing without a tall terminal buffer.

use std::collections::VecDeque;

use crate::width::display_width;
use ratatui::layout::Alignment;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::StyledGrapheme;

/// Materializes wrapped rows using the same `trim: false` word-packing rules as Ratatui's
/// `WordWrapper`, but emits each completed row immediately instead of rendering every row into one
/// `u16`-height `Buffer`.
pub(crate) fn wrap_line_rows(
    line: &Line<'_>,
    width: u16,
    style: ratatui::style::Style,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut pending_line = Vec::new();
    let mut pending_word = Vec::new();
    let mut pending_whitespace = VecDeque::new();
    let mut line_width = 0u16;
    let mut word_width = 0u16;
    let mut whitespace_width = 0u16;
    let mut non_whitespace_previous = false;

    for grapheme in line.styled_graphemes(style) {
        let is_whitespace = is_wrap_whitespace_grapheme(grapheme.symbol);
        let symbol_width = u16::try_from(display_width(grapheme.symbol)).unwrap_or(u16::MAX);
        if symbol_width > width {
            continue;
        }

        let untrimmed_overflow = pending_line.is_empty()
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > width;
        if non_whitespace_previous && is_whitespace || untrimmed_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width = line_width.saturating_add(whitespace_width);
            pending_line.append(&mut pending_word);
            line_width = line_width.saturating_add(word_width);
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;
        if line_full || pending_word_overflow {
            let mut remaining_width = width.saturating_sub(line_width);
            rows.push(line_from_graphemes(
                std::mem::take(&mut pending_line),
                line_width,
                width,
                line.alignment,
            ));
            line_width = 0;

            while let Some(grapheme) = pending_whitespace.front() {
                let grapheme_width =
                    u16::try_from(display_width(grapheme.symbol)).unwrap_or(u16::MAX);
                if grapheme_width > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(grapheme_width);
                remaining_width = remaining_width.saturating_sub(grapheme_width);
                pending_whitespace.pop_front();
            }
            if is_whitespace && pending_whitespace.is_empty() {
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            pending_whitespace.push_back(grapheme);
        } else {
            word_width = word_width.saturating_add(symbol_width);
            pending_word.push(grapheme);
        }
        non_whitespace_previous = !is_whitespace;
    }

    pending_line.extend(pending_whitespace);
    line_width = line_width.saturating_add(whitespace_width);
    pending_line.append(&mut pending_word);
    line_width = line_width.saturating_add(word_width);
    if !pending_line.is_empty() {
        rows.push(line_from_graphemes(
            pending_line,
            line_width,
            width,
            line.alignment,
        ));
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows
}

fn line_from_graphemes(
    graphemes: Vec<StyledGrapheme<'_>>,
    rendered_width: u16,
    area_width: u16,
    alignment: Option<Alignment>,
) -> Line<'static> {
    let leading_columns = match alignment {
        Some(Alignment::Center) => (area_width / 2).saturating_sub(rendered_width / 2),
        Some(Alignment::Right) => area_width.saturating_sub(rendered_width),
        Some(Alignment::Left) | None => 0,
    };
    let mut spans = Vec::new();
    if leading_columns > 0 {
        spans.push(Span::raw(" ".repeat(usize::from(leading_columns))));
    }
    for grapheme in graphemes {
        if display_width(grapheme.symbol) == 0 {
            continue;
        }
        if let Some(previous) = spans.last_mut()
            && previous.style == grapheme.style
        {
            previous.content.to_mut().push_str(grapheme.symbol);
        } else {
            spans.push(Span::styled(grapheme.symbol.to_string(), grapheme.style));
        }
    }
    Line::from(spans)
}
fn is_wrap_whitespace_grapheme(symbol: &str) -> bool {
    symbol == "\u{200b}" || symbol != "\u{00a0}" && symbol.chars().all(char::is_whitespace)
}

#[cfg(test)]
#[path = "rows_tests.rs"]
mod tests;
