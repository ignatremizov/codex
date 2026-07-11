//! Semantic terminal hyperlinks carried separately from visible TUI text.
//!
//! Layout code measures and wraps ordinary ratatui lines. Hyperlink annotations are applied only
//! when text reaches a terminal buffer or scrollback writer so OSC 8 bytes never affect geometry.

mod paragraph;

pub(crate) use paragraph::HyperlinkParagraph;

use std::collections::VecDeque;
use std::num::NonZeroU16;
use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::buffer::CellWidth;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::StyledGrapheme;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use unicode_segmentation::UnicodeSegmentation;
use url::Url;

use crate::line_truncation::line_width;
use crate::render::line_utils::line_to_borrowed;
use crate::render::line_utils::line_to_static;
use crate::width::display_width;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;

// Destinations are repeated in every linked buffer cell. Leave oversized URLs as plain text.
const MAX_HYPERLINK_DESTINATION_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalHyperlink {
    pub(crate) columns: Range<usize>,
    pub(crate) destination: String,
    destination_kind: DestinationKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DestinationKind {
    Web,
    TrustedFile,
}

impl TerminalHyperlink {
    pub(crate) fn web(columns: Range<usize>, destination: String) -> Self {
        Self {
            columns,
            destination,
            destination_kind: DestinationKind::Web,
        }
    }

    pub(crate) fn retarget_to_trusted_file(&mut self, destination: &Url) {
        // Keep file URLs out of the general Markdown link path. Only generated visualization links
        // are promoted to this destination kind.
        debug_assert_eq!(destination.scheme(), "file");
        self.destination = destination.to_string();
        self.destination_kind = DestinationKind::TrustedFile;
    }

    fn with_columns(&self, columns: Range<usize>) -> Self {
        Self {
            columns,
            destination: self.destination.clone(),
            destination_kind: self.destination_kind,
        }
    }

    fn terminal_destination(&self) -> Option<String> {
        match self.destination_kind {
            DestinationKind::Web => web_destination(&self.destination),
            DestinationKind::TrustedFile => trusted_file_destination(&self.destination),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct HyperlinkLine {
    pub(crate) line: Line<'static>,
    pub(crate) hyperlinks: Vec<TerminalHyperlink>,
}

impl HyperlinkLine {
    pub(crate) fn new(line: Line<'static>) -> Self {
        Self {
            line,
            hyperlinks: Vec::new(),
        }
    }

    pub(crate) fn width(&self) -> usize {
        line_width(&self.line)
    }

    pub(crate) fn push_span(&mut self, span: Span<'static>, destination: Option<&str>) {
        let start = self.width();
        let end = start + display_width(span.content.as_ref());
        self.line.push_span(span);
        if end > start
            && let Some(destination) = destination.and_then(web_destination)
        {
            self.hyperlinks
                .push(TerminalHyperlink::web(start..end, destination));
        }
    }

    pub(crate) fn style(mut self, style: ratatui::style::Style) -> Self {
        self.line = self.line.style(style);
        self
    }
}

impl From<Line<'static>> for HyperlinkLine {
    fn from(line: Line<'static>) -> Self {
        Self::new(line)
    }
}

impl From<&'static str> for HyperlinkLine {
    fn from(text: &'static str) -> Self {
        Self::new(Line::from(text))
    }
}

impl From<String> for HyperlinkLine {
    fn from(text: String) -> Self {
        Self::new(Line::from(text))
    }
}

pub(crate) fn visible_lines(lines: Vec<HyperlinkLine>) -> Vec<Line<'static>> {
    lines.into_iter().map(|line| line.line).collect()
}

pub(crate) fn visible_lines_ref(lines: &[HyperlinkLine]) -> Vec<Line<'_>> {
    lines
        .iter()
        .map(|line| line_to_borrowed(&line.line))
        .collect()
}

/// Wraps hyperlink-aware source lines into independently renderable viewport rows.
///
/// This work is intended to be cached by viewport renderers. Once wrapped, callers can slice the
/// returned rows before cloning visible text or applying hyperlink annotations, keeping repeated
/// scrolling work proportional to the viewport rather than the complete source.
pub(crate) fn wrap_hyperlink_lines(
    lines: &[HyperlinkLine],
    width: u16,
    style: ratatui::style::Style,
) -> Vec<HyperlinkLine> {
    if width == 0 {
        return Vec::new();
    }

    let mut wrapped = Vec::new();
    for source in lines {
        let rendered_lines = wrap_line_rows(&source.line, width, style);
        wrapped.extend(remap_wrapped_line(source, rendered_lines));
    }
    wrapped
}

/// Materializes wrapped rows using the same `trim: false` word-packing rules as Ratatui's
/// `WordWrapper`, but emits each completed row immediately instead of rendering every row into one
/// `u16`-height `Buffer`.
fn wrap_line_rows(line: &Line<'_>, width: u16, style: ratatui::style::Style) -> Vec<Line<'static>> {
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
                if line_full && non_whitespace_previous {
                    rows.push(Line::default());
                }
                non_whitespace_previous = false;
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

pub(crate) fn plain_hyperlink_lines(lines: Vec<Line<'static>>) -> Vec<HyperlinkLine> {
    lines.into_iter().map(HyperlinkLine::new).collect()
}

pub(crate) fn prefix_hyperlink_lines(
    lines: Vec<HyperlinkLine>,
    initial_prefix: Span<'static>,
    subsequent_prefix: Span<'static>,
) -> Vec<HyperlinkLine> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let prefix = if index == 0 {
                initial_prefix.clone()
            } else {
                subsequent_prefix.clone()
            };
            let shift = display_width(prefix.content.as_ref());
            let mut spans = Vec::with_capacity(line.line.spans.len() + 1);
            spans.push(prefix);
            spans.extend(line.line.spans);
            line.line = Line::from(spans).style(line.line.style);
            for hyperlink in &mut line.hyperlinks {
                hyperlink.columns = hyperlink.columns.start + shift..hyperlink.columns.end + shift;
            }
            line
        })
        .collect()
}

pub(crate) fn adaptive_wrap_hyperlink_lines(
    lines: &[HyperlinkLine],
    options: RtOptions<'static>,
) -> Vec<HyperlinkLine> {
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let options = if index == 0 {
            options.clone()
        } else {
            options
                .clone()
                .initial_indent(options.subsequent_indent.clone())
        };
        out.extend(remap_wrapped_line(
            line,
            adaptive_wrap_line(&line.line, options)
                .into_iter()
                .map(|wrapped| line_to_static(&wrapped))
                .collect(),
        ));
    }
    out
}

pub(crate) fn annotate_web_urls(lines: Vec<Line<'static>>) -> Vec<HyperlinkLine> {
    lines.into_iter().map(annotate_web_urls_in_line).collect()
}

pub(crate) fn annotate_web_urls_in_line(line: Line<'static>) -> HyperlinkLine {
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let mut out = HyperlinkLine::new(line);
    out.hyperlinks = web_links_in_text(&text);
    out
}

/// Re-attach source hyperlink ranges after visible-text wrapping has split a line.
///
/// Link text is matched in display order so a URL split across table rows retains the complete
/// destination on every rendered fragment. Whitespace inserted or removed at line boundaries is
/// ignored while matching; hyperlink destinations themselves are never reconstructed from output.
pub(crate) fn remap_wrapped_line(
    source: &HyperlinkLine,
    wrapped: Vec<Line<'static>>,
) -> Vec<HyperlinkLine> {
    let mut out = plain_hyperlink_lines(wrapped);
    if source.hyperlinks.is_empty() {
        return out;
    }
    // Ratatui does not materialize zero-width graphemes into buffer cells. Remove standalone
    // zero-width source clusters from the matching stream as well; they consume neither source nor
    // output columns, while non-zero-width clusters containing joiners or combining marks remain
    // intact.
    let source_text = line_text(&source.line)
        .graphemes(/*is_extended*/ true)
        .filter(|grapheme| display_width(grapheme) > 0)
        .collect::<String>();
    let mut source_byte = 0usize;
    let mut source_column = 0usize;
    let mut link_index = 0usize;
    let mut mapped_any = false;
    for line in &mut out {
        let rendered = line_text(&line.line);
        let remaining = &source_text[source_byte..];
        let Some((rendered_start, skipped_source_bytes)) =
            wrapped_fragment_match(&rendered, remaining, mapped_any)
        else {
            continue;
        };
        source_column += display_width(&remaining[..skipped_source_bytes]);
        source_byte += skipped_source_bytes;
        let mapped = &rendered[rendered_start..];
        let mut output_column = display_width(&rendered[..rendered_start]);
        for grapheme in mapped.graphemes(/*is_extended*/ true) {
            let width = display_width(grapheme);
            while source
                .hyperlinks
                .get(link_index)
                .is_some_and(|link| link.columns.end <= source_column)
            {
                link_index += 1;
            }
            if let Some(link) = source
                .hyperlinks
                .get(link_index)
                .filter(|link| link.columns.contains(&source_column))
            {
                push_link_range(line, output_column..output_column + width, link);
            }
            source_column += width;
            output_column += width;
        }
        source_byte += mapped.len();
        mapped_any = true;
    }
    out
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn wrapped_fragment_match(
    rendered: &str,
    source: &str,
    allow_source_whitespace_skip: bool,
) -> Option<(usize, usize)> {
    wrapped_fragment_match_impl(rendered, source, allow_source_whitespace_skip).0
}

/// Returns the wrapped-fragment match and a count of all materialized characters, prefix
/// inspections, comparisons, and byte-offset scans. Keeping the complete work count available
/// makes the linear bound directly testable without relying on wall-clock timing.
fn wrapped_fragment_match_impl(
    rendered: &str,
    source: &str,
    allow_source_whitespace_skip: bool,
) -> (Option<(usize, usize)>, usize) {
    let rendered_chars = rendered.chars().collect::<Vec<_>>();
    let mut inspected = rendered_chars.len();
    if rendered_chars.is_empty() {
        return (Some((0, 0)), inspected);
    }

    // Only a skippable source prefix and at most one rendered fragment beyond it can participate in
    // this match. Bounding materialization here avoids rescanning the complete remaining logical
    // line for every wrapped row.
    let mut source_chars = Vec::new();
    let mut skippable_source_chars = 0usize;
    let mut scanning_skippable_prefix = allow_source_whitespace_skip;
    for ch in source.chars() {
        inspected = inspected.saturating_add(1);
        if scanning_skippable_prefix {
            inspected = inspected.saturating_add(1);
            if is_wrap_whitespace(ch) {
                skippable_source_chars += 1;
            } else {
                scanning_skippable_prefix = false;
            }
        }
        source_chars.push(ch);
        if !scanning_skippable_prefix
            && source_chars.len() >= skippable_source_chars.saturating_add(rendered_chars.len())
        {
            break;
        }
    }
    let (source_match, comparisons) =
        last_pattern_match_at_or_before(&source_chars, &rendered_chars, skippable_source_chars);
    inspected = inspected.saturating_add(comparisons);
    if let Some(source_start_chars) = source_match {
        inspected =
            inspected.saturating_add(source_start_chars.saturating_add(1).min(source_chars.len()));
        return (
            Some((0, char_offset_to_byte(source, source_start_chars))),
            inspected,
        );
    }

    // No complete rendered fragment occurs after a legal source skip. Treat a leading rendered
    // prefix as wrapper-inserted alignment only when the rendered suffix is a source prefix.
    let source_after_skip = &source_chars[skippable_source_chars..];
    let source_prefix_len = source_after_skip.len().min(rendered_chars.len());
    let (overlap, comparisons) =
        suffix_prefix_overlap(&rendered_chars, &source_after_skip[..source_prefix_len]);
    inspected = inspected.saturating_add(comparisons);
    let matched = (overlap > 0).then(|| {
        let rendered_start_chars = rendered_chars.len() - overlap;
        inspected = inspected
            .saturating_add(
                rendered_start_chars
                    .saturating_add(1)
                    .min(rendered_chars.len()),
            )
            .saturating_add(
                skippable_source_chars
                    .saturating_add(1)
                    .min(source_chars.len()),
            );
        (
            char_offset_to_byte(rendered, rendered_start_chars),
            char_offset_to_byte(source, skippable_source_chars),
        )
    });
    (matched, inspected)
}

fn char_offset_to_byte(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map_or(text.len(), |(byte, _)| byte)
}

/// Finds the last pattern occurrence whose start is at or before `max_start`, using KMP.
fn last_pattern_match_at_or_before(
    text: &[char],
    pattern: &[char],
    max_start: usize,
) -> (Option<usize>, usize) {
    let (prefix, mut comparisons) = kmp_prefix(pattern);
    let mut matched = 0usize;
    let mut last_match = None;
    for (index, ch) in text.iter().enumerate() {
        while matched > 0 {
            comparisons = comparisons.saturating_add(1);
            if pattern[matched] == *ch {
                break;
            }
            matched = prefix[matched - 1];
        }
        comparisons = comparisons.saturating_add(1);
        if pattern[matched] == *ch {
            matched += 1;
        }
        if matched == pattern.len() {
            let start = index + 1 - pattern.len();
            if start <= max_start {
                last_match = Some(start);
            }
            matched = prefix[matched - 1];
        }
    }
    (last_match, comparisons)
}

/// Returns the longest suffix of `text` that is a prefix of `pattern`, using KMP.
fn suffix_prefix_overlap(text: &[char], pattern: &[char]) -> (usize, usize) {
    if pattern.is_empty() {
        return (0, 0);
    }
    let (prefix, mut comparisons) = kmp_prefix(pattern);
    let mut matched = 0usize;
    for (index, ch) in text.iter().enumerate() {
        while matched > 0 {
            comparisons = comparisons.saturating_add(1);
            if pattern[matched] == *ch {
                break;
            }
            matched = prefix[matched - 1];
        }
        comparisons = comparisons.saturating_add(1);
        if pattern[matched] == *ch {
            matched += 1;
        }
        if matched == pattern.len() {
            if index + 1 == text.len() {
                return (matched, comparisons);
            }
            matched = prefix[matched - 1];
        }
    }
    (matched, comparisons)
}

fn kmp_prefix(pattern: &[char]) -> (Vec<usize>, usize) {
    let mut prefix = vec![0; pattern.len()];
    let mut matched = 0usize;
    let mut comparisons = 0usize;
    for index in 1..pattern.len() {
        while matched > 0 {
            comparisons = comparisons.saturating_add(1);
            if pattern[index] == pattern[matched] {
                break;
            }
            matched = prefix[matched - 1];
        }
        comparisons = comparisons.saturating_add(1);
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    (prefix, comparisons)
}

fn is_wrap_whitespace(ch: char) -> bool {
    ch == '\u{200b}' || ch.is_whitespace() && ch != '\u{00a0}'
}

fn is_wrap_whitespace_grapheme(symbol: &str) -> bool {
    symbol == "\u{200b}" || symbol != "\u{00a0}" && symbol.chars().all(char::is_whitespace)
}

fn push_link_range(line: &mut HyperlinkLine, range: Range<usize>, link: &TerminalHyperlink) {
    if range.is_empty() {
        return;
    }
    if let Some(previous) = line.hyperlinks.last_mut()
        && previous.destination == link.destination
        && previous.destination_kind == link.destination_kind
        && previous.columns.end == range.start
    {
        previous.columns.end = range.end;
        return;
    }
    line.hyperlinks.push(link.with_columns(range));
}

pub(crate) fn web_links_in_text(text: &str) -> Vec<TerminalHyperlink> {
    let mut links = Vec::new();
    let mut search_from = 0usize;
    let mut source_byte = 0usize;
    let mut source_column = 0usize;
    for raw_token in text.split_whitespace() {
        let Some(relative_start) = text[search_from..].find(raw_token) else {
            continue;
        };
        let raw_start = search_from + relative_start;
        search_from = raw_start + raw_token.len();
        let trimmed_start = raw_token
            .find(|ch: char| !is_leading_punctuation(ch))
            .unwrap_or(raw_token.len());
        let trimmed_end = trailing_url_end(&raw_token[trimmed_start..]) + trimmed_start;
        if trimmed_start >= trimmed_end {
            continue;
        }
        let candidate = &raw_token[trimmed_start..trimmed_end];
        let Some(destination) = web_destination(candidate) else {
            continue;
        };
        let candidate_start = raw_start + trimmed_start;
        // Measure disjoint prefixes so scanning a draft with many URLs stays linear.
        source_column += display_width(&text[source_byte..candidate_start]);
        source_byte = candidate_start;
        let end = source_column + display_width(candidate);
        links.push(TerminalHyperlink::web(source_column..end, destination));
    }
    links
}

fn is_leading_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | '.' | ';' | '!' | '\'' | '"'
    )
}

fn trailing_url_end(candidate: &str) -> usize {
    // Count delimiter balances once rather than rescanning for every trailing closer.
    let mut balances = [0isize; 4];
    for ch in candidate.chars() {
        match ch {
            '(' => balances[0] += 1,
            ')' => balances[0] -= 1,
            '[' => balances[1] += 1,
            ']' => balances[1] -= 1,
            '{' => balances[2] += 1,
            '}' => balances[2] -= 1,
            '<' => balances[3] += 1,
            '>' => balances[3] -= 1,
            _ => {}
        }
    }
    let mut end = candidate.len();
    while end > 0 {
        let remaining = &candidate[..end];
        let Some(ch) = remaining.chars().next_back() else {
            break;
        };
        let balance = match ch {
            ')' => Some(&mut balances[0]),
            ']' => Some(&mut balances[1]),
            '}' => Some(&mut balances[2]),
            '>' => Some(&mut balances[3]),
            _ => None,
        };
        let trim = if let Some(balance) = balance {
            let unmatched = *balance < 0;
            *balance += 1;
            unmatched
        } else {
            matches!(ch, ',' | '.' | ';' | '!' | '\'' | '"')
        };
        if !trim {
            break;
        }
        end -= ch.len_utf8();
    }
    end
}

pub(crate) fn web_destination(destination: &str) -> Option<String> {
    let safe_destination = sanitized_destination(destination)?;
    let parsed = Url::parse(&safe_destination).ok()?;
    matches!(parsed.scheme(), "http" | "https")
        .then(|| parsed.host_str())
        .flatten()?;
    Some(safe_destination)
}

fn trusted_file_destination(destination: &str) -> Option<String> {
    let safe_destination = sanitized_destination(destination)?;
    let parsed = Url::parse(&safe_destination).ok()?;
    (parsed.scheme() == "file" && parsed.to_file_path().is_ok()).then_some(safe_destination)
}

fn sanitized_destination(destination: &str) -> Option<String> {
    if destination.len() > MAX_HYPERLINK_DESTINATION_BYTES {
        return None;
    }
    Some(destination.chars().filter(|ch| !ch.is_control()).collect())
}

pub(crate) fn osc8_hyperlink(destination: &str, text: &str) -> String {
    let Some(safe_destination) = web_destination(destination) else {
        return text.to_string();
    };
    format!("\x1b]8;;{safe_destination}\x07{text}\x1b]8;;\x07")
}

#[cfg(test)]
pub(crate) fn strip_osc8(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut stripped = String::with_capacity(text.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index..].starts_with(b"\x1b]8;;") {
            index += 5;
            while index < bytes.len() {
                if bytes[index] == b'\x07' {
                    index += 1;
                    break;
                }
                if index + 1 < bytes.len() && bytes[index] == b'\x1b' && bytes[index + 1] == b'\\' {
                    index += 2;
                    break;
                }
                index += 1;
            }
            continue;
        }
        let ch = text[index..]
            .chars()
            .next()
            .expect("current byte index starts a character");
        stripped.push(ch);
        index += ch.len_utf8();
    }

    stripped
}

pub(crate) fn decorate_spans(line: &HyperlinkLine) -> Vec<Span<'static>> {
    if line.hyperlinks.is_empty() {
        return line.line.spans.clone();
    }

    let mut out = Vec::new();
    let mut column = 0usize;
    let mut link_index = 0usize;
    let mut active_link_index = None;
    let mut active_destination: Option<String> = None;
    for span in &line.line.spans {
        for grapheme in span.content.graphemes(/*is_extended*/ true) {
            let width = display_width(grapheme);
            while line
                .hyperlinks
                .get(link_index)
                .is_some_and(|link| link.columns.end <= column)
            {
                link_index += 1;
            }
            let selected_link_index = line
                .hyperlinks
                .get(link_index)
                .and_then(|link| link.columns.contains(&column).then_some(link_index));
            if active_link_index != selected_link_index {
                if active_destination.is_some() {
                    append_to_last_span(&mut out, "\x1b]8;;\x07");
                }
                active_destination = selected_link_index
                    .and_then(|index| line.hyperlinks[index].terminal_destination());
                if let Some(destination) = active_destination.as_ref() {
                    push_styled_content(
                        &mut out,
                        &format!("\x1b]8;;{destination}\x07"),
                        span.style,
                    );
                }
                active_link_index = selected_link_index;
            }
            push_styled_content(&mut out, grapheme, span.style);
            column += width;
        }
    }
    if active_destination.is_some() {
        append_to_last_span(&mut out, "\x1b]8;;\x07");
    }
    out
}

fn push_styled_content(out: &mut Vec<Span<'static>>, content: &str, style: ratatui::style::Style) {
    if let Some(last) = out.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(content);
        return;
    }
    out.push(Span::styled(content.to_string(), style));
}

fn append_to_last_span(out: &mut [Span<'static>], content: &str) {
    if let Some(last) = out.last_mut() {
        last.content.to_mut().push_str(content);
    }
}

pub(crate) fn mark_buffer_hyperlinks(
    buf: &mut Buffer,
    area: Rect,
    lines: &[HyperlinkLine],
    scroll_rows: usize,
) {
    if area.width == 0 || area.height == 0 || lines.iter().all(|line| line.hyperlinks.is_empty()) {
        return;
    }
    let viewport_end = scroll_rows.saturating_add(usize::from(area.height));
    let mut logical_row = 0usize;
    for line in lines {
        if logical_row >= viewport_end {
            break;
        }
        let paragraph =
            Paragraph::new(Text::from(line_to_borrowed(&line.line))).wrap(Wrap { trim: false });
        let rendered_height = paragraph.line_count(area.width).max(/*other*/ 1);
        if line.hyperlinks.is_empty() || logical_row.saturating_add(rendered_height) <= scroll_rows
        {
            logical_row += rendered_height;
            continue;
        }

        let layout_area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            area.width,
            u16::try_from(rendered_height).unwrap_or(u16::MAX),
        );
        let mut layout = Buffer::empty(layout_area);
        paragraph.render(layout_area, &mut layout);
        let rendered_lines = (0..layout_area.height)
            .map(|row| {
                let mut trailing_columns = 0usize;
                let text = (0..layout_area.width)
                    .filter_map(|column| {
                        if trailing_columns > 0 {
                            trailing_columns -= 1;
                            return None;
                        }
                        let cell = &layout[(column, row)];
                        if cell.diff_option == CellDiffOption::Skip {
                            return None;
                        }
                        trailing_columns = usize::from(cell.cell_width()).saturating_sub(1);
                        Some(cell.symbol())
                    })
                    .collect::<String>();
                Line::from(text.trim_end().to_string())
            })
            .collect();
        for (row, rendered) in remap_wrapped_line(line, rendered_lines).iter().enumerate() {
            let row = logical_row + row;
            if row < scroll_rows || row >= viewport_end {
                continue;
            }
            for link in &rendered.hyperlinks {
                let Some(destination) = link.terminal_destination() else {
                    continue;
                };
                let mut trailing_columns = 0usize;
                for column in link.columns.clone() {
                    if trailing_columns > 0 {
                        trailing_columns -= 1;
                        continue;
                    }
                    let x = area.x + column as u16;
                    let y = area.y + (row - scroll_rows) as u16;
                    let cell = &mut buf[(x, y)];
                    if cell.diff_option == CellDiffOption::Skip {
                        continue;
                    }
                    trailing_columns = usize::from(cell.cell_width()).saturating_sub(1);
                    let symbol = format!("\x1b]8;;{destination}\x07{}\x1b]8;;\x07", cell.symbol());
                    let width = NonZeroU16::new(cell.cell_width()).unwrap_or(NonZeroU16::MIN);
                    cell.set_symbol(&symbol)
                        .set_diff_option(CellDiffOption::ForcedWidth(width));
                }
            }
        }
        logical_row += rendered_height;
    }
}

/// Applies hyperlinks to independently materialized rows.
///
/// Unlike [`mark_buffer_hyperlinks`], this does not wrap or measure its input: every
/// `HyperlinkLine` corresponds to exactly one row already rendered into `area`.
pub(crate) fn mark_buffer_hyperlinks_in_rows(buf: &mut Buffer, area: Rect, rows: &[HyperlinkLine]) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for (row, line) in rows.iter().take(usize::from(area.height)).enumerate() {
        for link in &line.hyperlinks {
            let end = link.columns.end.min(usize::from(area.width));
            let mut trailing_columns = 0usize;
            for column in link.columns.start..end {
                if trailing_columns > 0 {
                    trailing_columns -= 1;
                    continue;
                }
                let x = area.x + column as u16;
                let y = area.y + row as u16;
                let cell = &mut buf[(x, y)];
                if cell.diff_option == CellDiffOption::Skip {
                    continue;
                }
                trailing_columns = usize::from(cell.cell_width()).saturating_sub(1);
                let symbol = link.terminal_destination().map_or_else(
                    || cell.symbol().to_string(),
                    |destination| format!("\x1b]8;;{destination}\x07{}\x1b]8;;\x07", cell.symbol()),
                );
                let width = NonZeroU16::new(cell.cell_width()).unwrap_or(NonZeroU16::MIN);
                cell.set_symbol(&symbol)
                    .set_diff_option(CellDiffOption::ForcedWidth(width));
            }
        }
    }
}

pub(crate) fn mark_url_hyperlink(buf: &mut Buffer, area: Rect, destination: &str) {
    mark_matching_cells(buf, area, destination, |cell| {
        cell.fg == Color::Cyan && cell.modifier.contains(Modifier::UNDERLINED)
    });
}

pub(crate) fn mark_underlined_hyperlink(buf: &mut Buffer, area: Rect, destination: &str) {
    mark_matching_cells(buf, area, destination, |cell| {
        cell.modifier.contains(Modifier::UNDERLINED)
    });
}

fn mark_matching_cells(
    buf: &mut Buffer,
    area: Rect,
    destination: &str,
    matches: impl Fn(&ratatui::buffer::Cell) -> bool,
) {
    if web_destination(destination).is_none() {
        return;
    }
    for position in area.positions() {
        let cell = &mut buf[position];
        if cell.diff_option != CellDiffOption::Skip
            && !cell.symbol().trim().is_empty()
            && matches(cell)
        {
            let width = NonZeroU16::new(cell.cell_width()).unwrap_or(NonZeroU16::MIN);
            let symbol = osc8_hyperlink(destination, cell.symbol());
            cell.set_symbol(&symbol)
                .set_diff_option(CellDiffOption::ForcedWidth(width));
        }
    }
}

#[cfg(test)]
#[path = "terminal_hyperlinks_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Style;

    #[test]
    fn only_web_destinations_receive_osc8() {
        assert!(osc8_hyperlink("https://example.com/a", "a").contains("\x1b]8;;"));
        assert_eq!(osc8_hyperlink("mailto:a@example.com", "a"), "a");
        assert_eq!(
            osc8_hyperlink("https://example.com/\u{7}safe", "a"),
            "\x1b]8;;https://example.com/safe\x07a\x1b]8;;\x07"
        );
        assert_eq!(
            strip_osc8(&osc8_hyperlink("https://example.com/a", "visible")),
            "visible"
        );
    }

    #[test]
    fn discovers_punctuated_web_url_columns() {
        assert_eq!(
            web_links_in_text("See (https://example.com/a)."),
            vec![TerminalHyperlink::web(
                /*columns*/ 5..26,
                "https://example.com/a".to_string(),
            )]
        );
    }

    #[test]
    fn hyperlink_columns_follow_a_long_prefix_without_wrapping() {
        let prefix = "a".repeat(65_536);
        let destination = "https://example.com/long-prefix";
        let text = format!("{prefix} {destination}");

        assert_eq!(
            HyperlinkLine::new(Line::from(text.clone())).width(),
            text.len()
        );
        assert_eq!(
            web_links_in_text(&text),
            vec![TerminalHyperlink::web(
                /*columns*/ 65_537..65_537 + destination.len(),
                destination.to_string(),
            )]
        );
    }

    #[test]
    fn preserves_balanced_parentheses_in_bare_web_urls() {
        let destination = "https://en.wikipedia.org/wiki/Function_(mathematics)";
        assert_eq!(
            web_links_in_text(&format!("See ({destination}).")),
            vec![TerminalHyperlink::web(
                /*columns*/ 5..5 + usize::from(destination.cell_width()),
                destination.to_string(),
            )]
        );
    }

    #[test]
    fn decorates_a_contiguous_web_link_with_one_osc8_pair() {
        let destination = "https://example.com/a/very/long/path";
        let line = HyperlinkLine {
            line: Line::from(destination),
            hyperlinks: vec![TerminalHyperlink::web(
                /*columns*/ 0..usize::from(destination.cell_width()),
                destination.to_string(),
            )],
        };

        assert_eq!(
            decorate_spans(&line),
            vec![Span::from(osc8_hyperlink(destination, destination))]
        );
        assert_eq!(
            decorate_spans(&HyperlinkLine::new(Line::from("not linked"))),
            vec![Span::from("not linked")]
        );
    }

    #[test]
    fn wrapping_maps_repeated_link_labels_by_source_position() {
        let mut source = HyperlinkLine::new(Line::from("here here"));
        source.hyperlinks.push(TerminalHyperlink::web(
            /*columns*/ 5..9,
            "https://example.com".to_string(),
        ));

        let wrapped = remap_wrapped_line(&source, vec![Line::from("here here")]);

        assert_eq!(
            wrapped[0].hyperlinks,
            vec![TerminalHyperlink::web(
                /*columns*/ 5..9,
                "https://example.com".to_string(),
            )]
        );
    }

    #[test]
    fn wrapping_maps_multiple_links_across_indented_unicode_lines() {
        let text = "alpha 😀here middle there end";
        let first_start = text.find("here").expect("first link");
        let second_start = text.find("there").expect("second link");
        let first_column = usize::from(text[..first_start].cell_width());
        let second_column = usize::from(text[..second_start].cell_width());
        let mut source = HyperlinkLine::new(Line::from(text));
        source.hyperlinks.push(TerminalHyperlink::web(
            first_column..first_column + usize::from("here".cell_width()),
            "https://example.com/first".to_string(),
        ));
        source.hyperlinks.push(TerminalHyperlink::web(
            second_column..second_column + usize::from("there".cell_width()),
            "https://example.com/second".to_string(),
        ));

        let wrapped = remap_wrapped_line(
            &source,
            vec![
                Line::from("  alpha 😀here"),
                Line::from("    middle there end"),
            ],
        );

        assert_eq!(
            wrapped,
            vec![
                HyperlinkLine {
                    line: Line::from("  alpha 😀here"),
                    hyperlinks: vec![TerminalHyperlink::web(
                        /*columns*/ 10..14,
                        "https://example.com/first".to_string(),
                    )],
                },
                HyperlinkLine {
                    line: Line::from("    middle there end"),
                    hyperlinks: vec![TerminalHyperlink::web(
                        /*columns*/ 11..16,
                        "https://example.com/second".to_string(),
                    )],
                },
            ]
        );
    }

    #[test]
    fn remapping_uses_grapheme_widths_around_zwj_clusters() {
        let source = HyperlinkLine {
            line: Line::from("A 👩‍💻 B"),
            hyperlinks: vec![
                TerminalHyperlink {
                    columns: 0..1,
                    destination: "https://example.com/before".to_string(),
                    destination_kind: DestinationKind::Web,
                },
                TerminalHyperlink {
                    columns: 2..4,
                    destination: "https://example.com/within".to_string(),
                    destination_kind: DestinationKind::Web,
                },
                TerminalHyperlink {
                    columns: 5..6,
                    destination: "https://example.com/after".to_string(),
                    destination_kind: DestinationKind::Web,
                },
            ],
        };

        let wrapped = remap_wrapped_line(&source, vec![source.line.clone()]);

        assert_eq!(wrapped[0].hyperlinks, source.hyperlinks);
    }

    #[test]
    fn remapping_uses_grapheme_widths_around_combining_clusters() {
        let source = HyperlinkLine {
            line: Line::from("A e\u{301} B"),
            hyperlinks: vec![
                TerminalHyperlink {
                    columns: 0..1,
                    destination: "https://example.com/before".to_string(),
                    destination_kind: DestinationKind::Web,
                },
                TerminalHyperlink {
                    columns: 2..3,
                    destination: "https://example.com/within".to_string(),
                    destination_kind: DestinationKind::Web,
                },
                TerminalHyperlink {
                    columns: 4..5,
                    destination: "https://example.com/after".to_string(),
                    destination_kind: DestinationKind::Web,
                },
            ],
        };

        let wrapped = remap_wrapped_line(&source, vec![source.line.clone()]);

        assert_eq!(wrapped[0].hyperlinks, source.hyperlinks);
    }

    #[test]
    fn remapping_consumes_standalone_zero_width_clusters_across_wraps() {
        for omitted in ["\u{301}", "\u{200d}"] {
            let source = HyperlinkLine {
                line: Line::from(format!("{omitted}A B")),
                hyperlinks: vec![TerminalHyperlink {
                    columns: 2..3,
                    destination: "https://example.com/after".to_string(),
                    destination_kind: DestinationKind::Web,
                }],
            };

            let wrapped = remap_wrapped_line(&source, vec![Line::from("A"), Line::from("B")]);

            assert!(wrapped[0].hyperlinks.is_empty());
            assert_eq!(
                wrapped[1].hyperlinks,
                vec![TerminalHyperlink {
                    columns: 0..1,
                    destination: "https://example.com/after".to_string(),
                    destination_kind: DestinationKind::Web,
                }],
                "standalone cluster {omitted:?} must not block source advancement"
            );
        }
    }

    #[test]
    fn wrapped_fragment_matching_inspects_a_large_whitespace_prefix_linearly() {
        let whitespace_count = 100_000usize;
        let source = format!("{}tail", " ".repeat(whitespace_count));

        let (matched, inspected) = wrapped_fragment_match_impl(
            "tail", &source, /*allow_source_whitespace_skip*/ true,
        );

        assert_eq!(matched, Some((0, whitespace_count)));
        assert!(
            inspected <= 5 * (source.chars().count() + "tail".chars().count()),
            "all matching work should remain linear, inspected {inspected} characters/comparisons"
        );
    }

    #[test]
    fn wrapped_fragment_no_match_inspects_adversarial_prefix_linearly() {
        let whitespace_count = 100_000usize;
        let rendered = format!("{}b", " ".repeat(10_000));
        let source = format!("{}a", " ".repeat(whitespace_count));

        let (matched, inspected) = wrapped_fragment_match_impl(
            &rendered, &source, /*allow_source_whitespace_skip*/ true,
        );

        assert_eq!(matched, None);
        assert!(
            inspected <= 8 * (source.chars().count() + rendered.chars().count()),
            "no-match search should remain linear, inspected {inspected} characters/comparisons"
        );
    }

    #[test]
    fn buffer_hyperlinks_follow_word_wrapping() {
        let destination = "https://example.com/path";
        let mut line = HyperlinkLine::new(Line::from(format!("See {destination} now")));
        line.hyperlinks.push(TerminalHyperlink::web(
            /*columns*/ 4..4 + usize::from(destination.cell_width()),
            destination.to_string(),
        ));
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 18, /*height*/ 4,
        );
        let mut buf = Buffer::empty(area);

        HyperlinkParagraph::new(&[line], Style::default()).render(area, &mut buf);

        let linked_text = area
            .positions()
            .filter_map(|position| {
                let symbol = buf[position].symbol();
                symbol
                    .contains(&format!("\x1b]8;;{destination}\x07"))
                    .then(|| strip_osc8(symbol))
            })
            .collect::<String>();
        assert_eq!(linked_text, destination);
    }

    #[test]
    fn buffer_hyperlinks_follow_scrolled_wrapped_rows() {
        let hidden_destination = "https://example.com/hidden";
        let visible_destination = "https://example.com/visible";
        let trailing_destination = "https://example.com/trailing";

        let mut hidden = HyperlinkLine::new(Line::default());
        hidden.push_span("hidden".into(), Some(hidden_destination));
        let mut visible = HyperlinkLine::new(Line::from("prefix "));
        visible.push_span("visible-link".into(), Some(visible_destination));
        let mut trailing = HyperlinkLine::new(Line::default());
        trailing.push_span("trailing".into(), Some(trailing_destination));
        let lines = vec![hidden, visible, trailing];

        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 8, /*height*/ 2,
        );
        let backend = crate::test_backend::VT100Backend::new(area.width, area.height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(area);
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                HyperlinkParagraph::new(&lines, Style::default())
                    .scroll(/*rows*/ 2)
                    .render(area, buf);

                let linked_text = area
                    .positions()
                    .filter_map(|position| {
                        let symbol = buf[position].symbol();
                        symbol
                            .contains(&format!("\x1b]8;;{visible_destination}\x07"))
                            .then(|| strip_osc8(symbol))
                    })
                    .collect::<String>();
                assert_eq!(linked_text, "visible-link");
            })
            .expect("render scrolled hyperlinks");

        insta::assert_snapshot!(
            "buffer_hyperlinks_follow_scrolled_wrapped_rows",
            terminal.backend()
        );
    }

    #[test]
    fn buffer_hyperlinks_follow_wrapped_wide_glyphs() {
        let destination = "https://example.com/wide";
        let mut line = HyperlinkLine::new(Line::from("前文 "));
        line.push_span("漢字漢字".into(), Some(destination));
        line.push_span(" 後文".into(), /*destination*/ None);
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 6, /*height*/ 4,
        );
        let mut buf = Buffer::empty(area);

        Paragraph::new(Text::from(line.line.clone()))
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        mark_buffer_hyperlinks(&mut buf, area, &[line], /*scroll_rows*/ 0);

        let linked_text = area
            .positions()
            .filter_map(|position| {
                let symbol = buf[position].symbol();
                symbol
                    .contains(&format!("\x1b]8;;{destination}\x07"))
                    .then(|| strip_osc8(symbol))
            })
            .collect::<String>();
        assert_eq!(linked_text, "漢字漢字");
    }

    #[test]
    fn buffer_hyperlinks_follow_wrapped_halfwidth_dakuten() {
        let destination = "https://example.com/dakuten";
        let mut line = HyperlinkLine::new(Line::from("ｶﾞ "));
        line.push_span("ﾊﾟlink".into(), Some(destination));
        line.push_span(" tail".into(), /*destination*/ None);
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 5, /*height*/ 4,
        );
        let mut buf = Buffer::empty(area);

        Paragraph::new(Text::from(line.line.clone()))
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        mark_buffer_hyperlinks(&mut buf, area, &[line], /*scroll_rows*/ 0);

        let linked_text = area
            .positions()
            .filter_map(|position| {
                let symbol = buf[position].symbol();
                symbol
                    .contains(&format!("\x1b]8;;{destination}\x07"))
                    .then(|| strip_osc8(symbol))
            })
            .collect::<String>();
        assert_eq!(linked_text, "ﾊﾟlink");
    }

    #[test]
    fn forced_width_hyperlinks_render_wide_and_halfwidth_cells_snapshot() {
        let destination = "https://example.com/rendered";
        let mut line = HyperlinkLine::new(Line::from("prefix "));
        line.push_span("漢字 ｶﾞ".into(), Some(destination));
        line.push_span(" tail".into(), /*destination*/ None);

        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 14, /*height*/ 3,
        );
        let backend = crate::test_backend::VT100Backend::new(area.width, area.height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(area);

        terminal
            .draw(|frame| {
                Paragraph::new(Text::from(line.line.clone()))
                    .wrap(Wrap { trim: false })
                    .render(area, frame.buffer_mut());
                mark_buffer_hyperlinks(
                    frame.buffer_mut(),
                    area,
                    &[line.clone()],
                    /*scroll_rows*/ 0,
                );
            })
            .expect("render hyperlinks");

        insta::assert_snapshot!(
            "forced_width_hyperlinks_render_wide_and_halfwidth_cells",
            terminal.backend()
        );
    }

    #[test]
    fn buffer_hyperlinks_preserve_visible_cell_width_for_ratatui_diff() {
        let destination = "https://example.com/dakuten";
        let mut line = HyperlinkLine::new(Line::from("ｶﾞ tail"));
        line.hyperlinks.push(TerminalHyperlink::web(
            /*columns*/ 0..2,
            destination.to_string(),
        ));
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 7, /*height*/ 1,
        );
        let previous = Buffer::with_lines(["       "]);
        let mut next = Buffer::empty(area);

        Paragraph::new(Text::from(line.line.clone())).render(area, &mut next);
        mark_buffer_hyperlinks(&mut next, area, &[line], /*scroll_rows*/ 0);

        assert_eq!(next[(0, 0)].cell_width(), 2);
        assert!(matches!(
            next[(0, 0)].diff_option,
            CellDiffOption::ForcedWidth(width) if width.get() == 2
        ));
        assert_eq!(
            previous
                .diff_iter(&next)
                .map(|(x, _, cell)| (x, strip_osc8(cell.symbol())))
                .collect::<Vec<_>>(),
            vec![
                (0, "ｶﾞ".to_string()),
                (3, "t".to_string()),
                (4, "a".to_string()),
                (5, "i".to_string()),
                (6, "l".to_string()),
            ]
        );
    }

    #[test]
    fn matching_hyperlinks_preserve_visible_cell_width_for_ratatui_diff() {
        let destination = "https://example.com/dakuten";
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 7, /*height*/ 1,
        );
        let previous = Buffer::with_lines(["       "]);
        let mut next = Buffer::empty(area);
        next.set_string(
            /*x*/ 0,
            /*y*/ 0,
            "ｶﾞ tail",
            Style::default().add_modifier(Modifier::UNDERLINED),
        );

        mark_underlined_hyperlink(&mut next, area, destination);

        assert_eq!(next[(0, 0)].cell_width(), 2);
        assert!(matches!(
            next[(0, 0)].diff_option,
            CellDiffOption::ForcedWidth(width) if width.get() == 2
        ));
        assert_eq!(
            previous
                .diff_iter(&next)
                .map(|(x, _, cell)| (x, strip_osc8(cell.symbol())))
                .collect::<Vec<_>>(),
            vec![
                (0, "ｶﾞ".to_string()),
                (2, " ".to_string()),
                (3, "t".to_string()),
                (4, "a".to_string()),
                (5, "i".to_string()),
                (6, "l".to_string()),
            ]
        );
    }

    #[test]
    fn trusted_file_destination_receives_osc8_without_enabling_plain_file_links() {
        let temp_dir = tempfile::tempdir().expect("temp directory");
        let file_url = Url::from_file_path(temp_dir.path().join("viewer.html"))
            .expect("test path should convert to file URL");
        let mut link = TerminalHyperlink::web(
            /*columns*/ 0..4,
            "https://codex.invalid/viewer".to_string(),
        );
        link.retarget_to_trusted_file(&file_url);
        let line = HyperlinkLine {
            line: Line::from("view"),
            hyperlinks: vec![link],
        };

        assert_eq!(
            decorate_spans(&line),
            vec![Span::from(format!(
                "\x1b]8;;{file_url}\x07view\x1b]8;;\x07"
            ))]
        );
        assert_eq!(osc8_hyperlink(file_url.as_str(), "view"), "view");
    }

    #[test]
    fn prewrapped_hyperlinks_follow_a_fitting_whitespace_only_row() {
        let destination = "https://example.com/after-spaces";
        let rows = vec![
            HyperlinkLine::from("  "),
            HyperlinkLine {
                line: Line::from("linked"),
                hyperlinks: vec![TerminalHyperlink {
                    columns: 0..6,
                    destination: destination.to_string(),
                    destination_kind: DestinationKind::Web,
                }],
            },
            HyperlinkLine::from("after"),
        ];
        let area = Rect::new(0, 0, 8, 3);
        let mut buf = Buffer::empty(area);
        Paragraph::new(Text::from(visible_lines(rows.clone()))).render(area, &mut buf);

        mark_buffer_hyperlinks_in_rows(&mut buf, area, &rows);

        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert!(buf[(0, 1)].symbol().contains(destination));
        assert!(buf[(5, 1)].symbol().contains(destination));
        assert_eq!(buf[(0, 2)].symbol(), "a");
    }

    #[test]
    fn prewrapped_hyperlink_after_whitespace_lands_at_viewport_edge() {
        let destination = "https://example.com/edge";
        let rows = vec![
            HyperlinkLine::from("  "),
            HyperlinkLine {
                line: Line::from("edge"),
                hyperlinks: vec![TerminalHyperlink {
                    columns: 0..4,
                    destination: destination.to_string(),
                    destination_kind: DestinationKind::Web,
                }],
            },
        ];
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        Paragraph::new(Text::from(visible_lines(rows.clone()))).render(area, &mut buf);

        mark_buffer_hyperlinks_in_rows(&mut buf, area, &rows);

        assert!(buf[(0, 1)].symbol().contains(destination));
        assert!(buf[(3, 1)].symbol().contains(destination));
    }

    #[test]
    fn prewrapped_centered_rows_preserve_alignment_and_hyperlink_columns() {
        let destination = "https://example.com";
        let text = "abcdefghijklm";
        let source = HyperlinkLine {
            line: Line::from(text).alignment(Alignment::Center),
            hyperlinks: vec![TerminalHyperlink {
                columns: 0..display_width(text),
                destination: destination.to_string(),
                destination_kind: DestinationKind::Web,
            }],
        };

        let wrapped = wrap_hyperlink_lines(&[source], /*width*/ 10, Style::default());

        assert_eq!(line_text(&wrapped[0].line), "abcdefghij");
        // Ratatui's pinned `get_line_offset` uses `width / 2 - line_width / 2`.
        assert_eq!(line_text(&wrapped[1].line), "    klm");
        assert_eq!(wrapped[0].hyperlinks[0].columns, 0..10);
        assert_eq!(wrapped[1].hyperlinks[0].columns, 4..7);

        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new(wrapped[1].line.clone()).render(area, &mut buf);
        mark_buffer_hyperlinks_in_rows(&mut buf, area, &wrapped[1..2]);
        assert_eq!(buf[(3, 0)].symbol(), " ");
        assert!(buf[(4, 0)].symbol().contains(destination));
        assert!(buf[(6, 0)].symbol().contains(destination));
    }

    #[test]
    fn prewrapped_right_aligned_rows_preserve_alignment_and_hyperlink_columns() {
        let destination = "https://example.com";
        let text = "abcdefghijklm";
        let source = HyperlinkLine {
            line: Line::from(text).alignment(Alignment::Right),
            hyperlinks: vec![TerminalHyperlink {
                columns: 0..display_width(text),
                destination: destination.to_string(),
                destination_kind: DestinationKind::Web,
            }],
        };

        let wrapped = wrap_hyperlink_lines(&[source], /*width*/ 10, Style::default());

        assert_eq!(line_text(&wrapped[0].line), "abcdefghij");
        assert_eq!(line_text(&wrapped[1].line), "       klm");
        assert_eq!(wrapped[0].hyperlinks[0].columns, 0..10);
        assert_eq!(wrapped[1].hyperlinks[0].columns, 7..10);

        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new(wrapped[1].line.clone()).render(area, &mut buf);
        mark_buffer_hyperlinks_in_rows(&mut buf, area, &wrapped[1..2]);
        assert_eq!(buf[(6, 0)].symbol(), " ");
        assert!(buf[(7, 0)].symbol().contains(destination));
        assert!(buf[(9, 0)].symbol().contains(destination));
    }

    #[test]
    fn wrapping_mixed_text_and_trailing_whitespace_matches_word_wrapper_rows() {
        let wrapped = wrap_hyperlink_lines(
            &[HyperlinkLine::from("abc   ")],
            /*width*/ 3,
            Style::default(),
        );

        assert_eq!(
            wrapped
                .iter()
                .map(|line| line_text(&line.line))
                .collect::<Vec<_>>(),
            vec!["abc", "", "  "]
        );
    }

    #[test]
    fn wrapping_whitespace_only_line_matches_word_wrapper_rows() {
        let fitting = wrap_hyperlink_lines(
            &[HyperlinkLine::from("  ")],
            /*width*/ 3,
            Style::default(),
        );
        let overflowing = wrap_hyperlink_lines(
            &[HyperlinkLine::from("      ")],
            /*width*/ 3,
            Style::default(),
        );

        assert_eq!(
            fitting
                .iter()
                .map(|line| line_text(&line.line))
                .collect::<Vec<_>>(),
            vec!["  "]
        );
        assert_eq!(
            overflowing
                .iter()
                .map(|line| line_text(&line.line))
                .collect::<Vec<_>>(),
            vec!["   ", "  "]
        );
    }

    #[test]
    fn wrapping_preserves_hyperlinks_on_rendered_trailing_spaces() {
        let destination = "https://example.com/spaces";
        let source = HyperlinkLine {
            line: Line::from("abc   "),
            hyperlinks: vec![TerminalHyperlink {
                columns: 3..6,
                destination: destination.to_string(),
                destination_kind: DestinationKind::Web,
            }],
        };

        let wrapped = wrap_hyperlink_lines(&[source], /*width*/ 3, Style::default());

        assert_eq!(line_text(&wrapped[2].line), "  ");
        assert_eq!(
            wrapped[2].hyperlinks,
            vec![TerminalHyperlink {
                columns: 0..2,
                destination: destination.to_string(),
                destination_kind: DestinationKind::Web,
            }]
        );
    }

    #[test]
    fn one_logical_line_wraps_beyond_u16_max_without_losing_its_tail() {
        let destination = "https://example.com/huge";
        let row_count = usize::from(u16::MAX) + 2;
        let mut text = "x".repeat(row_count - 1);
        text.push('Z');
        let source = HyperlinkLine {
            line: Line::from(Span::styled(text, Style::default().fg(Color::LightMagenta))),
            hyperlinks: vec![TerminalHyperlink {
                columns: 0..row_count,
                destination: destination.to_string(),
                destination_kind: DestinationKind::Web,
            }],
        };

        let wrapped = wrap_hyperlink_lines(&[source], /*width*/ 1, Style::default());

        assert_eq!(wrapped.len(), row_count);
        let tail = wrapped
            .last()
            .expect("the final wrapped row should be reachable");
        assert_eq!(line_text(&tail.line), "Z");
        assert_eq!(tail.line.spans[0].style.fg, Some(Color::LightMagenta));
        assert_eq!(
            tail.hyperlinks,
            vec![TerminalHyperlink {
                columns: 0..1,
                destination: destination.to_string(),
                destination_kind: DestinationKind::Web,
            }]
        );
    }
}
