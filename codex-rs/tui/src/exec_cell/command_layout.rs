//! Syntax-aware command rows without rewriting command source.
//!
//! Statement boundaries are soft display breaks on shared logical source lines. Shell
//! literals remain opaque; literal Node stdin heredocs use the bundled JavaScript grammar.
//! Unknown or oversized syntax retains the original highlighting and wrapping.

use crate::render::highlight::MAX_HIGHLIGHT_LINE_BYTES;
use crate::render::highlight::exceeds_highlight_limits;
use crate::render::highlight::highlight_bash_to_lines;
use crate::render::highlight::highlight_code_to_lines;
use crate::render::highlight::javascript_statement_breaks;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::LogicalLineSource;
use codex_shell_command::bash::try_parse_shell;
use ratatui::text::Line;

pub(super) struct CommandLayout {
    pub(super) lines: Vec<Line<'static>>,
    breaks: Vec<Vec<usize>>,
}

impl CommandLayout {
    pub(super) fn new(script: &str) -> Self {
        let mut result = Self {
            lines: highlight_bash_to_lines(script),
            breaks: Vec::new(),
        };
        if exceeds_highlight_limits(script.len(), script.lines().count())
            || script
                .lines()
                .any(|line| line.len() > MAX_HIGHLIGHT_LINE_BYTES)
            || (!script.contains(';') && !script.contains("<<"))
        {
            return result;
        }
        let Some(tree) = try_parse_shell(script) else {
            return result;
        };
        let root = tree.root_node();
        if root.has_error() {
            return result;
        }
        let mut breaks = Vec::new();
        let mut nodes = vec![root];
        while let Some(node) = nodes.pop() {
            match node.kind() {
                ";" | ";;" | ";&" | ";;&" => breaks.push(node.end_byte()),
                "heredoc_redirect" => {
                    // Only a literal Node stdin script has a reliable embedded language.
                    // File arguments, other descriptors and expanding heredocs stay shell text.
                    let mut cursor = node.walk();
                    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
                    let marker = children
                        .iter()
                        .find(|child| child.kind() == "heredoc_start");
                    let end = children.iter().find(|child| child.kind() == "heredoc_end");
                    let command = node
                        .parent()
                        .and_then(|parent| parent.child_by_field_name("body"))
                        .and_then(|body| script.get(body.byte_range()))
                        .and_then(shlex::split);
                    let redirect_count = node.parent().map(|parent| {
                        let mut cursor = parent.walk();
                        parent
                            .children_by_field_name("redirect", &mut cursor)
                            .count()
                    });
                    let is_stdin = redirect_count == Some(1)
                        && node.child_by_field_name("argument").is_none()
                        && node.child_by_field_name("redirect").is_none()
                        && node
                            .child_by_field_name("descriptor")
                            .is_none_or(|descriptor| &script[descriptor.byte_range()] == "0");
                    if let (Some(marker), Some(end), Some(command)) = (marker, end, command)
                        && is_stdin
                        && command.first().is_some_and(|name| {
                            matches!(name.rsplit('/').next(), Some("node" | "nodejs"))
                        })
                        && command
                            .iter()
                            .skip(/*n*/ 1)
                            .all(|arg| arg == "-" || arg.starts_with("--input-type="))
                        && script[marker.byte_range()].starts_with(['\'', '"'])
                        && let Some(newline) = script[node.byte_range()].find('\n')
                    {
                        let start = node.start_byte() + newline + 1;
                        // <<- includes closing-marker tabs in the body's tree-sitter range.
                        // Keep the entire closing source row outside the embedded highlighter.
                        let end = script[..end.start_byte()]
                            .rfind('\n')
                            .map_or(/*default*/ 0, |index| index + 1);
                        let code = &script[start..end];
                        if let Some(code_breaks) = javascript_statement_breaks(code) {
                            breaks.extend(code_breaks.into_iter().map(|index| start + index));
                        }
                        let first_row = script[..start]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count();
                        let highlighted = if code.is_empty() {
                            Vec::new()
                        } else {
                            highlight_code_to_lines(code, "javascript")
                        };
                        for (target, highlighted) in
                            result.lines.iter_mut().skip(first_row).zip(highlighted)
                        {
                            *target = highlighted;
                        }
                    }
                    continue;
                }
                // Do not interpret punctuation inside literals, substitutions or arithmetic.
                "string"
                | "raw_string"
                | "ansi_c_string"
                | "translated_string"
                | "word"
                | "comment"
                | "regex"
                | "heredoc_body"
                | "expansion"
                | "arithmetic_expansion"
                | "arithmetic_command"
                | "test_command"
                | "command_substitution"
                | "process_substitution" => continue,
                "c_style_for_statement" => {
                    if let Some(body) = node.child_by_field_name("body") {
                        nodes.push(body);
                    }
                    continue;
                }
                _ => {}
            }
            let mut cursor = node.walk();
            nodes.extend(node.children(&mut cursor));
        }
        breaks.sort_unstable();
        breaks.dedup();
        let mut offset = 0;
        for line in script.split_inclusive('\n') {
            let text = line.trim_end_matches(['\n', '\r']);
            let first = breaks.partition_point(|index| *index <= offset);
            let last = breaks.partition_point(|index| *index < offset + text.len());
            result.breaks.push(
                breaks[first..last]
                    .iter()
                    .copied()
                    .filter(|index| !script[*index..offset + text.len()].trim().is_empty())
                    .map(|index| index - offset)
                    .collect(),
            );
            offset += line.len();
        }
        result
    }

    pub(super) fn into_lines(self) -> Vec<HyperlinkLine> {
        let mut result = Vec::new();
        for (index, line) in self.lines.into_iter().enumerate() {
            let mut source = LogicalLineSource::from_line(&line);
            source.display_breaks = self.breaks.get(index).cloned().unwrap_or_default().into();
            let mut start = 0;
            for end in source
                .display_breaks
                .iter()
                .copied()
                .chain(std::iter::once(source.text.len()))
            {
                let mut fragment = source.clone();
                fragment.range = start..end;
                let mut row = HyperlinkLine::new(source.styled_range(fragment.range.clone()));
                row.source = Some(fragment);
                result.push(row);
                start = end;
                while source
                    .text
                    .as_bytes()
                    .get(start)
                    .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
                {
                    start += 1;
                }
            }
        }
        result
    }
}

#[cfg(test)]
#[path = "command_layout_tests.rs"]
mod tests;
