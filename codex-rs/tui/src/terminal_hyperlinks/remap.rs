//! Legacy hyperlink projection for unannotated rows; source-backed wrapping keeps exact ranges.

use super::*;

pub(super) fn remap_wrapped_line(
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

#[cfg(test)]
#[path = "remap_tests.rs"]
mod tests;
