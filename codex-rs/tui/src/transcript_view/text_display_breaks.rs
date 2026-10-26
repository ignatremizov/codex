//! Split presentation-only statement rows before ordinary wrapping and tab projection.
//!
//! Each segment keeps its original source offsets. Hidden indentation is omitted only from
//! painting, so selecting across statement rows still includes every original space and tab.

use crate::terminal_hyperlinks::HyperlinkLine;
use crate::width::display_width;

use super::TextRow;
use super::logical::LogicalLine;
use super::logical::slice_line;

pub(super) fn layout_line(logical: &LogicalLine, text_start: usize, width: u16) -> Vec<TextRow> {
    if logical.origin.display_breaks.is_empty() {
        return super::layout_line(logical, text_start, width);
    }
    let origin = &logical.origin;
    let text = &origin.text[origin.range.clone()];
    let mut boundaries = vec![0];
    boundaries.extend(
        origin
            .display_breaks
            .iter()
            .copied()
            .filter(|&offset| {
                offset > origin.range.start
                    && offset < origin.range.end
                    && origin.text.is_char_boundary(offset)
            })
            .map(|offset| offset - origin.range.start),
    );
    boundaries.dedup();
    boundaries.push(text.len());

    let mut rows = Vec::new();
    let mut previous_end = 0;
    let mut column = 0;
    for (index, pair) in boundaries.windows(2).enumerate() {
        let mut start = pair[0];
        let end = pair[1];
        if index > 0 || origin.display_breaks.contains(&origin.range.start) {
            start += text[start..end]
                .bytes()
                .take_while(|byte| matches!(*byte, b' ' | b'\t'))
                .count();
        }
        column += display_width(&text[previous_end..start]);
        let end_column = column + display_width(&text[start..end]);
        let mut line = HyperlinkLine::new(slice_line(&logical.line.line, start..end));
        line.line.alignment = logical.line.line.alignment;
        line.hyperlinks = logical
            .line
            .hyperlinks
            .iter()
            .filter_map(|link| {
                let first = link.columns.start.max(column);
                let last = link.columns.end.min(end_column);
                (first < last).then(|| link.with_columns(first - column..last - column))
            })
            .collect();
        let mut source = origin.clone();
        source.range = origin.range.start + start..origin.range.start + end;
        let segment = LogicalLine {
            line,
            initial_indent: if index == 0 {
                logical.initial_indent.clone()
            } else {
                logical.subsequent_indent.clone()
            },
            subsequent_indent: logical.subsequent_indent.clone(),
            wrap_policy: logical.wrap_policy,
            right_reserve: logical.right_reserve,
            origin: source,
        };
        rows.extend(super::layout_line(&segment, text_start + start, width));
        column = end_column;
        previous_end = end;
    }
    rows
}
