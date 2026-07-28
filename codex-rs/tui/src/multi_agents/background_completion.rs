use codex_app_server_protocol::CollabAgentState;
use codex_app_server_protocol::CollabAgentStatus;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::SubAgentCompletionStatus;
use codex_protocol::protocol::sub_agent_completion_status_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_transcript_parts;
use ratatui::style::Stylize;
use ratatui::text::Span;

use super::CollabAgentHistoryCell;
use super::collab_event;
use super::completion_agent_lines;
use super::title_text;

pub(crate) fn background_completion_history_cell_from_agent_message(
    id: &str,
    text: &str,
    phase: Option<&MessagePhase>,
    agent_response_preview_lines: usize,
) -> Option<CollabAgentHistoryCell> {
    if phase != Some(&MessagePhase::Commentary) {
        return None;
    }
    let completion_status = sub_agent_completion_status_from_response_item_id(id)?;
    let (agent_reference, payload) = sub_agent_completion_transcript_parts(text)?;
    let (status, message) = match completion_status {
        SubAgentCompletionStatus::Completed => (
            CollabAgentStatus::Completed,
            (!payload.is_empty()).then(|| payload.to_string()),
        ),
        SubAgentCompletionStatus::Errored => {
            (CollabAgentStatus::Errored, Some(payload.to_string()))
        }
        SubAgentCompletionStatus::Shutdown => (CollabAgentStatus::Shutdown, None),
        SubAgentCompletionStatus::NotFound => (CollabAgentStatus::NotFound, None),
    };
    let agent_reference = agent_reference.trim();
    let label = if agent_reference.is_empty() {
        "agent"
    } else {
        agent_reference
    };
    let status = CollabAgentState { status, message };
    Some(collab_event(
        title_text("Agent finished"),
        completion_agent_lines(
            vec![Span::from(label.to_string()).cyan()],
            &status,
            agent_response_preview_lines,
        ),
    ))
}
