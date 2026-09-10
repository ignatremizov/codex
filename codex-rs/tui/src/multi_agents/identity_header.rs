//! Shared rich agent identity, independent of message verbs and canonical audit output.

use codex_protocol::openai_models::ReasoningEffort;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::text::Span;

/// Borrowed presentation fields; refs must come from trusted attribution or aliases.
pub(crate) struct IdentityHeader<'a> {
    pub(crate) fallback: &'a str,
    pub(crate) nickname: Option<&'a str>,
    pub(crate) role: Option<&'a str>,
    pub(crate) task_path: Option<&'a str>,
    pub(crate) agent_ref: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) reasoning_effort: Option<&'a ReasoningEffort>,
}

#[cfg(test)]
#[path = "identity_header_tests.rs"]
mod tests;

impl IdentityHeader<'_> {
    pub(crate) fn render(self) -> Line<'static> {
        let nonempty = |value: &&str| !value.is_empty();
        let nickname = self.nickname.map(str::trim).filter(nonempty);
        let name = nickname.unwrap_or(self.fallback);
        let mut metadata = String::new();
        if let Some(role) = self.role.map(str::trim).filter(nonempty) {
            metadata.push_str(&format!(" [{}]", role.escape_debug()));
        }
        if let Some(task_path) = self.task_path.map(str::trim).filter(nonempty) {
            metadata.push_str(&format!(" {}", task_path.escape_debug()));
        }
        if let Some(agent_ref) = self.agent_ref.map(str::trim).filter(nonempty) {
            metadata.push_str(&format!(" ({})", agent_ref.escape_debug()));
        }
        match (
            self.model.map(str::trim).filter(nonempty),
            self.reasoning_effort,
        ) {
            (Some(model), Some(effort)) => {
                metadata.push_str(&format!(" ({} {effort})", model.escape_debug()));
            }
            (Some(model), None) => {
                metadata.push_str(&format!(" ({})", model.escape_debug()));
            }
            (None, Some(effort)) => metadata.push_str(&format!(" ({effort})")),
            (None, None) => {}
        }
        let mut identity = Span::from(name.escape_debug().to_string())
            .fg(crate::agent_color::nickname_color(name));
        if nickname.is_some() {
            identity = identity.bold();
        }
        vec![identity, metadata.dim()].into()
    }
}
