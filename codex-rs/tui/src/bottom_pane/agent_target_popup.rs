//! Autocomplete popup for `/agent` targets and control tokens.

use std::ops::Range;

use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::ThreadId;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::WidgetRef;

use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::ColumnWidthConfig;
use super::selection_popup_common::ColumnWidthMode;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::measure_rows_height_with_col_width_mode;
use super::selection_popup_common::render_rows_with_col_width_mode;
use crate::render::Insets;
use crate::render::RectExt;

const AGENT_TARGET_COLUMN_WIDTH: ColumnWidthConfig = ColumnWidthConfig::new(
    ColumnWidthMode::AutoAllRows,
    /*name_column_width*/ None,
);
pub(crate) const AGENT_TARGET_ACTION_CHOICES: [(&str, &str); 5] = [
    ("queue", "Queue a follow-up for an agent"),
    ("interrupt", "Interrupt an agent"),
    ("close", "Close an agent"),
    ("resume", "Resume or adopt an agent"),
    ("observe", "Change response observation"),
];
pub(crate) const AGENT_OBSERVATION_MODE_CHOICES: [(&str, &str); 3] = [
    ("passive", "Deliver the final response without waking"),
    ("wake", "Deliver the final response and wake"),
    (
        "presentation",
        "Keep the final response out of model context",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentPromptTarget {
    pub(crate) thread_id: Option<ThreadId>,
    pub(crate) selector: String,
    pub(crate) label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentTargetCompletionScope {
    Any,
    ExistingTarget,
    ObservationMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentTargetCompletion {
    pub(crate) range: Range<usize>,
    pub(crate) query: String,
    pub(crate) scope: AgentTargetCompletionScope,
    pub(crate) action: Option<&'static str>,
}

pub(crate) struct AgentTargetPopup {
    targets: Vec<AgentPromptTarget>,
    query: String,
    scope: AgentTargetCompletionScope,
    state: ScrollState,
}

impl AgentTargetPopup {
    pub(crate) fn new(
        targets: Vec<AgentPromptTarget>,
        query: &str,
        scope: AgentTargetCompletionScope,
    ) -> Self {
        let mut popup = Self {
            targets,
            query: String::new(),
            scope,
            state: ScrollState::new(),
        };
        popup.set_query(query);
        popup
    }

    pub(crate) fn set_targets(&mut self, targets: Vec<AgentPromptTarget>) {
        if self.targets == targets {
            return;
        }
        self.targets = targets;
        self.clamp_selection();
    }

    pub(crate) fn set_scope(&mut self, scope: AgentTargetCompletionScope) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.state.reset();
        self.clamp_selection();
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.state.reset();
        self.clamp_selection();
    }

    pub(crate) fn selected_target(&self) -> Option<AgentPromptTarget> {
        let matches = self.filtered_targets();
        self.state
            .selected_idx
            .and_then(|index| matches.get(index).cloned())
    }

    pub(crate) fn move_up(&mut self) {
        let len = self.filtered_targets().len();
        self.state.move_up_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    pub(crate) fn move_down(&mut self) {
        let len = self.filtered_targets().len();
        self.state.move_down_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    pub(crate) fn calculate_required_height(&self, width: u16) -> u16 {
        measure_rows_height_with_col_width_mode(
            &self.rows(),
            &self.state,
            MAX_POPUP_ROWS,
            width,
            AGENT_TARGET_COLUMN_WIDTH,
        )
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered_targets().len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    fn filtered_targets(&self) -> Vec<AgentPromptTarget> {
        let query = self.query.trim().to_ascii_lowercase();
        self.targets
            .iter()
            .filter(|target| {
                (self.scope != AgentTargetCompletionScope::ExistingTarget
                    || target.thread_id.is_some())
                    && if self.scope == AgentTargetCompletionScope::ObservationMode {
                        query.is_empty()
                            || target.selector.to_ascii_lowercase().starts_with(&query)
                            || target.label.to_ascii_lowercase().contains(&query)
                    } else {
                        target_matches_query(target, &query)
                    }
            })
            .cloned()
            .collect()
    }

    fn rows(&self) -> Vec<GenericDisplayRow> {
        let query_len = self.query.trim().chars().count();
        let query = self.query.trim().to_ascii_lowercase();
        self.filtered_targets()
            .into_iter()
            .map(|target| {
                let description = target.thread_id.map_or_else(
                    || target.label.clone(),
                    |thread_id| format!("{}  {thread_id}", target.label),
                );
                let match_indices = (!query.is_empty()
                    && target.selector.to_ascii_lowercase().starts_with(&query))
                .then(|| (0..query_len).collect());
                GenericDisplayRow {
                    name: target.selector,
                    name_prefix_spans: Vec::new(),
                    match_indices,
                    display_shortcut: None,
                    description: Some(description),
                    category_tag: None,
                    wrap_indent: None,
                    is_disabled: false,
                    disabled_reason: None,
                }
            })
            .collect()
    }
}

impl WidgetRef for AgentTargetPopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        render_rows_with_col_width_mode(
            area.inset(Insets::tlbr(
                /*top*/ 0, /*left*/ 2, /*bottom*/ 0, /*right*/ 0,
            )),
            buf,
            &self.rows(),
            &self.state,
            MAX_POPUP_ROWS,
            match self.scope {
                AgentTargetCompletionScope::Any => {
                    "no matching agents, actions, or configured roles"
                }
                AgentTargetCompletionScope::ExistingTarget => "no matching agents",
                AgentTargetCompletionScope::ObservationMode => {
                    "no matching response observation modes"
                }
            },
            AGENT_TARGET_COLUMN_WIDTH,
        );
    }
}

/// Returns an editable `/agent` selector token at the caret.
pub(crate) fn agent_target_completion(
    first_line: &str,
    cursor: usize,
) -> Option<AgentTargetCompletion> {
    const COMMAND: &str = "/agent";
    let tail = first_line.strip_prefix(COMMAND)?;
    if tail.is_empty() || !tail.starts_with(char::is_whitespace) {
        return None;
    }
    if cursor > first_line.len() || !first_line.is_char_boundary(cursor) {
        return None;
    }

    let target_start = COMMAND.len() + (tail.len() - tail.trim_start().len());
    if cursor < target_start {
        return None;
    }
    let first_end = token_end(first_line, target_start);
    if cursor <= first_end {
        let range = target_start..first_end;
        return Some(AgentTargetCompletion {
            query: first_line[range.clone()].to_string(),
            range,
            scope: AgentTargetCompletionScope::Any,
            action: None,
        });
    }

    let action = canonical_agent_target_action(&first_line[target_start..first_end])?;

    let action_tail = &first_line[first_end..];
    if !action_tail.starts_with(char::is_whitespace) {
        return None;
    }
    let target_start = first_end + (action_tail.len() - action_tail.trim_start().len());
    if cursor < target_start {
        return None;
    }
    let target_end = token_end(first_line, target_start);
    if cursor <= target_end {
        let range = target_start..target_end;
        return Some(AgentTargetCompletion {
            query: first_line[range.clone()].to_string(),
            range,
            scope: AgentTargetCompletionScope::ExistingTarget,
            action: Some(action),
        });
    }

    if action != "observe" {
        return None;
    }
    let target_tail = &first_line[target_end..];
    if !target_tail.starts_with(char::is_whitespace) {
        return None;
    }
    let mode_start = target_end + (target_tail.len() - target_tail.trim_start().len());
    if cursor < mode_start {
        return None;
    }
    let mode_end = token_end(first_line, mode_start);
    if cursor > mode_end {
        return None;
    }
    let range = mode_start..mode_end;
    Some(AgentTargetCompletion {
        query: first_line[range.clone()].to_string(),
        range,
        scope: AgentTargetCompletionScope::ObservationMode,
        action: Some(action),
    })
}

pub(super) fn token_end(input: &str, start: usize) -> usize {
    let mut cursor = start;
    let mut quoted = false;
    let mut escaped = false;
    while let Some(ch) = input[cursor..].chars().next() {
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if !quoted && ch.is_whitespace() {
            break;
        }
        cursor += ch.len_utf8();
    }
    cursor
}

fn target_matches_query(target: &AgentPromptTarget, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    if let Some(query) = query.strip_prefix("id:") {
        return target.thread_id.is_some_and(|thread_id| {
            thread_id
                .to_string()
                .to_ascii_lowercase()
                .starts_with(trim_quoted_query(query))
        });
    }
    if let Some(query) = query.strip_prefix("ref:") {
        return target_ref(target)
            .is_some_and(|agent_ref| agent_ref.starts_with(trim_quoted_query(query)));
    }
    if let Some(query) = query.strip_prefix("nick:") {
        return target.thread_id.is_some()
            && target
                .label
                .to_ascii_lowercase()
                .contains(trim_quoted_query(query));
    }
    if let Some(query) = query.strip_prefix("role:") {
        return target.thread_id.is_none()
            && target.selector != "new"
            && !is_agent_target_action(&target.selector)
            && target
                .label
                .to_ascii_lowercase()
                .contains(trim_quoted_query(query));
    }

    target_ref(target).is_some_and(|agent_ref| agent_ref.starts_with(query))
        || target.selector.to_ascii_lowercase().starts_with(query)
        || target.thread_id.is_some_and(|thread_id| {
            thread_id
                .to_string()
                .to_ascii_lowercase()
                .starts_with(query)
        })
        || target.label.to_ascii_lowercase().contains(query)
}

fn target_ref(target: &AgentPromptTarget) -> Option<&str> {
    target.thread_id?;
    if !target.selector.is_empty() && target.selector.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(&target.selector);
    }
    target
        .selector
        .eq_ignore_ascii_case(MAIN_AGENT_NICKNAME)
        .then_some("1")
}

fn trim_quoted_query(query: &str) -> &str {
    query
        .strip_prefix('"')
        .unwrap_or(query)
        .trim_end_matches('"')
}

pub(crate) fn is_agent_target_action(value: &str) -> bool {
    canonical_agent_target_action(value).is_some()
}

fn canonical_agent_target_action(value: &str) -> Option<&'static str> {
    AGENT_TARGET_ACTION_CHOICES
        .iter()
        .find_map(|(action, _label)| (*action == value).then_some(*action))
}

#[cfg(test)]
#[path = "agent_target_popup_tests.rs"]
mod tests;
