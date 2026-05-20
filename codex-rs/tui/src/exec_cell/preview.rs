//! Presentation-only head/tail previews with source-backed wrapping.
//!
//! A zero limit retains every row. Storage bounds belong to `LiveCommandOutput`, never to
//! these display preferences. Omitted screen rows and omitted storage lines are distinct units.

use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::remap_source_wrapped_line;
use crate::ui_consts::TRANSCRIPT_HINT;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line_with_source;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::collections::VecDeque;
use textwrap::WordSplitter;

pub(crate) struct OutputPreview {
    pub(crate) lines: Vec<HyperlinkLine>,
    pub(crate) hidden: bool,
}

pub(crate) fn output_preview(
    lines: impl IntoIterator<Item = HyperlinkLine>,
    width: usize,
    limit: usize,
    omitted_storage_lines: usize,
) -> OutputPreview {
    let width = width.max(/*other*/ 1);
    let mut head = Vec::new();
    let mut tail = VecDeque::new();
    let mut total = 0usize;
    // Keep a full limit until overflow is known, then reserve one row for the omission hint.
    let head_count = limit.saturating_sub(/*rhs*/ 1).div_ceil(/*rhs*/ 2);
    for line in lines {
        let wrapped = word_wrap_line_with_source(
            &line.line,
            RtOptions::new(width).word_splitter(WordSplitter::NoHyphenation),
        );
        for row in remap_source_wrapped_line(&line, wrapped) {
            total = total.saturating_add(/*rhs*/ 1);
            if limit == 0 || head.len() < head_count {
                head.push(row);
            } else {
                tail.push_back(row);
                if tail.len() > limit.saturating_sub(head_count) {
                    tail.pop_front();
                }
            }
        }
    }
    if limit == 0 || total <= limit {
        head.extend(tail);
        return OutputPreview {
            lines: head,
            hidden: false,
        };
    }
    let tail_count = limit.saturating_sub(1 + head_count);
    while tail.len() > tail_count {
        tail.pop_front();
    }
    let omitted = total.saturating_sub(head.len() + tail.len());
    let storage = if omitted_storage_lines == 0 {
        String::new()
    } else {
        format!("; {omitted_storage_lines} lines not retained")
    };
    let hint = format!("… +{omitted} rows{storage} ({TRANSCRIPT_HINT})");
    head.push(truncate_line_with_ellipsis_if_overflow(Line::from(hint.dim()), width).into());
    head.extend(tail);
    OutputPreview {
        lines: head,
        hidden: true,
    }
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod tests;
