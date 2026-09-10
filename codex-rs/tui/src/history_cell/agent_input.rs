//! Rich presentation of trusted agent input, separate from human and assistant messages.

use codex_app_server_protocol::AgentInputAttribution;
use codex_app_server_protocol::AgentInputIdentity;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use ratatui::style::Stylize as _;
use ratatui::text::Line;

use super::HistoryCell;
use crate::multi_agents::IdentityHeader;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

#[derive(Debug)]
pub(crate) struct AgentInputHistoryCell {
    attribution: AgentInputAttribution,
    title: Line<'static>,
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
            title.spans.push(" → ".into());
            title
                .spans
                .extend(identity_label(&attribution.recipient).spans);
            title.spans.push(" (presentation only)".italic());
        }
        title.spans.push(" sends:".bold());
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
            [self.title.clone()],
            RtOptions::new(width.max(1) as usize)
                .initial_indent("• ".dim().into())
                .subsequent_indent("  ".into()),
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
        let mut lines = vec![self.title.to_string().into()];
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

fn identity_label(identity: &AgentInputIdentity) -> Line<'static> {
    IdentityHeader {
        fallback: &identity.thread_id,
        nickname: identity.nickname.as_deref(),
        role: identity.role.as_deref(),
        task_path: identity.task_path.as_deref(),
        agent_ref: identity.agent_ref.as_deref(),
        model: identity.model.as_deref(),
        reasoning_effort: identity.reasoning_effort.as_ref(),
    }
    .render()
}

#[cfg(test)]
#[path = "agent_input_tests.rs"]
mod tests;
