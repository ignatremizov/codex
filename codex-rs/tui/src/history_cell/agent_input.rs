//! Rich presentation of trusted agent input, separate from human and assistant messages.

use codex_app_server_protocol::AgentInputAttribution;
use codex_app_server_protocol::AgentInputIdentity;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use ratatui::style::Stylize as _;
use ratatui::text::Line;

use super::HistoryCell;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

#[derive(Debug)]
pub(crate) struct AgentInputHistoryCell {
    attribution: AgentInputAttribution,
    title: String,
    payload: Vec<String>,
    preview_rows: usize,
}

impl AgentInputHistoryCell {
    pub(crate) fn new(
        attribution: AgentInputAttribution,
        input: Vec<UserInput>,
        fallback_text: String,
        viewed_thread: Option<ThreadId>,
    ) -> Self {
        let mut title = identity_label(&attribution.sender);
        if viewed_thread.is_some_and(|id| id.to_string() != attribution.recipient.thread_id) {
            title.push_str(&format!(
                " → {} (presentation only)",
                identity_label(&attribution.recipient),
            ));
        }
        title.push_str(" sends:");
        let payload = if input.is_empty() {
            vec![fallback_text]
        } else {
            input
                .into_iter()
                .map(|item| match item {
                    UserInput::Text { text, .. } => text,
                    // Attachment bytes remain in the typed history item; rendering an inline
                    // data URL here would turn a normal attachment into pages of base64.
                    UserInput::Image { .. } => "[image]".to_string(),
                    UserInput::LocalImage { path, .. } => format!("[image] {}", path.display()),
                    UserInput::Audio { .. } => "[audio]".to_string(),
                    UserInput::LocalAudio { path } => format!("[audio] {}", path.display()),
                    UserInput::Skill { name, path } => format!("[skill {name}] {}", path.display()),
                    UserInput::Mention { name, path } => format!("[mention {name}] {path}"),
                })
                .collect()
        };
        Self {
            attribution,
            title,
            payload,
            preview_rows: 0,
        }
    }

    pub(crate) fn with_response_preview_lines(mut self, preview_rows: usize) -> Self {
        self.preview_rows = preview_rows;
        self
    }

    fn audit_identity(&self) -> String {
        format!(
            "Sender: {} · Recipient: {} · Sender turn: {}",
            self.attribution.sender.thread_id,
            self.attribution.recipient.thread_id,
            self.attribution.sender_turn_id,
        )
    }
    fn render_lines(&self, width: u16, preview_rows: usize) -> Vec<Line<'static>> {
        let mut lines = word_wrap_lines(
            [Line::from(self.title.clone().cyan())],
            RtOptions::new(width.max(1) as usize),
        );
        let mut payload_lines = Vec::new();
        for (index, text) in self
            .payload
            .iter()
            .flat_map(|text| text.split('\n'))
            .enumerate()
        {
            payload_lines.extend(word_wrap_lines(
                [Line::from(text.to_string())],
                RtOptions::new(width.max(1) as usize)
                    .initial_indent(if index == 0 {
                        "  └ ".dim().into()
                    } else {
                        "    ".into()
                    })
                    .subsequent_indent("    ".into()),
            ));
        }
        lines.extend(crate::multi_agents::cap_preview_rows(
            payload_lines,
            preview_rows,
            /*first_detail*/ true,
            "",
        ));
        lines
    }
}

impl HistoryCell for AgentInputHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.render_lines(width, self.preview_rows)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![self.title.clone().into()];
        lines.extend(
            self.payload
                .iter()
                .flat_map(|text| text.split('\n'))
                .map(|text| Line::from(text.to_string())),
        );
        lines.push(self.audit_identity().into());
        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.render_lines(width, /*preview_rows*/ 0);
        lines.extend(word_wrap_lines(
            [Line::from(self.audit_identity().dim())],
            RtOptions::new(width.max(1) as usize),
        ));
        lines
    }
}

fn identity_label(identity: &AgentInputIdentity) -> String {
    let mut label = identity
        .nickname
        .as_deref()
        .unwrap_or(&identity.thread_id)
        .escape_debug()
        .to_string();
    if let Some(role) = &identity.role {
        label.push_str(&format!(" [{}]", role.escape_debug()));
    }
    if let Some(task_path) = &identity.task_path {
        label.push_str(&format!(" {}", task_path.escape_debug()));
    }
    if let Some(agent_ref) = &identity.agent_ref {
        label.push_str(&format!(" ({})", agent_ref.escape_debug()));
    }
    match (&identity.model, &identity.reasoning_effort) {
        (Some(model), Some(effort)) => {
            label.push_str(&format!(" ({} {effort})", model.escape_debug()));
        }
        (Some(model), None) => label.push_str(&format!(" ({})", model.escape_debug())),
        (None, Some(effort)) => label.push_str(&format!(" ({effort})")),
        (None, None) => {}
    }
    label
}

#[cfg(test)]
#[path = "agent_input_tests.rs"]
mod tests;
