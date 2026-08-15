//! Semantic highlight ranges for the editable `/agent` command.

use std::ops::Range;

use super::agent_target_popup::AGENT_OBSERVATION_MODE_CHOICES;
use super::agent_target_popup::AgentPromptTarget;
use super::agent_target_popup::is_agent_target_action;
use super::agent_target_popup::token_end;

const AGENT_COMMAND: &str = "/agent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentCommandHighlightKind {
    Command,
    Action,
    KnownTarget,
    UnknownTarget,
    Option,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentCommandHighlight {
    pub(super) range: Range<usize>,
    pub(super) kind: AgentCommandHighlightKind,
}

pub(super) fn agent_command_highlights(
    input: &str,
    targets: &[AgentPromptTarget],
) -> Vec<AgentCommandHighlight> {
    let first_line = input.lines().next().unwrap_or(input);
    let Some(tail) = first_line.strip_prefix(AGENT_COMMAND) else {
        return Vec::new();
    };
    if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
        return Vec::new();
    }

    let mut highlights = vec![AgentCommandHighlight {
        range: 0..AGENT_COMMAND.len(),
        kind: AgentCommandHighlightKind::Command,
    }];
    let token_ranges = command_token_ranges(first_line, AGENT_COMMAND.len());
    let Some(first_range) = token_ranges.first() else {
        return highlights;
    };
    let first = &first_line[first_range.clone()];
    let mut index = 1;
    let mut action = None;

    if first == "new" || is_agent_target_action(first) {
        action = Some(first);
        highlights.push(AgentCommandHighlight {
            range: first_range.clone(),
            kind: AgentCommandHighlightKind::Action,
        });
        if first != "new" {
            let Some(target_range) = token_ranges.get(index) else {
                return highlights;
            };
            highlights.push(target_highlight(first_line, target_range.clone(), targets));
            index += 1;
        }
    } else {
        highlights.push(target_highlight(first_line, first_range.clone(), targets));
        if token_ranges
            .get(index)
            .is_some_and(|range| &first_line[range.clone()] == "close")
        {
            let range = token_ranges[index].clone();
            highlights.push(AgentCommandHighlight {
                range,
                kind: AgentCommandHighlightKind::Action,
            });
            action = Some("close");
            index += 1;
        }
    }

    while let Some(range) = token_ranges.get(index) {
        let token = &first_line[range.clone()];
        let recognized = is_response_option(token)
            || is_fork_option(token)
            || (action == Some("observe")
                && AGENT_OBSERVATION_MODE_CHOICES
                    .iter()
                    .any(|(mode, _description)| token == *mode));
        if !recognized {
            break;
        }
        highlights.push(AgentCommandHighlight {
            range: range.clone(),
            kind: AgentCommandHighlightKind::Option,
        });
        index += 1;
    }

    highlights
}

fn command_token_ranges(input: &str, mut cursor: usize) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    while cursor < input.len() {
        while let Some(ch) = input[cursor..].chars().next()
            && ch.is_whitespace()
        {
            cursor += ch.len_utf8();
        }
        if cursor == input.len() {
            break;
        }
        let end = token_end(input, cursor);
        ranges.push(cursor..end);
        cursor = end;
    }
    ranges
}

fn target_highlight(
    input: &str,
    range: Range<usize>,
    targets: &[AgentPromptTarget],
) -> AgentCommandHighlight {
    let token = &input[range.clone()];
    let kind = if targets.iter().any(|target| target_matches(target, token)) {
        AgentCommandHighlightKind::KnownTarget
    } else {
        AgentCommandHighlightKind::UnknownTarget
    };
    AgentCommandHighlight { range, kind }
}

fn target_matches(target: &AgentPromptTarget, token: &str) -> bool {
    let token = token.trim_matches('"');
    if target.selector.eq_ignore_ascii_case(token) {
        return true;
    }
    if let Some(agent_ref) = token.strip_prefix("ref:")
        && target.selector == agent_ref
    {
        return true;
    }
    if let Some(thread_id) = target.thread_id {
        let thread_id = thread_id.to_string();
        if token == thread_id || token.strip_prefix("id:") == Some(thread_id.as_str()) {
            return true;
        }
    }
    let name = token
        .strip_prefix("nick:")
        .or_else(|| token.strip_prefix("role:"))
        .unwrap_or(token)
        .trim_matches('"');
    if target.selector.eq_ignore_ascii_case(name) {
        return true;
    }
    let label = target
        .label
        .split_once(" [")
        .map_or(target.label.as_str(), |(label, _role)| label);
    let label = label
        .split_once(" ·")
        .map_or(label, |(label, _state)| label);
    label.eq_ignore_ascii_case(name)
}

fn is_response_option(token: &str) -> bool {
    let Some(flags) = token.strip_prefix("w:") else {
        return false;
    };
    let mut previous = None;
    !flags.is_empty()
        && flags.chars().all(|flag| {
            let position = "cfmqx".find(flag);
            let valid = position
                .is_some_and(|position| previous.is_none_or(|previous| previous < position));
            previous = position;
            valid
        })
}

fn is_fork_option(token: &str) -> bool {
    let Some(value) = token.strip_prefix("fork:") else {
        return false;
    };
    matches!(value, "none" | "all") || value.parse::<u32>().is_ok_and(|turns| turns > 0)
}

#[cfg(test)]
#[path = "agent_command_highlight_tests.rs"]
mod tests;
