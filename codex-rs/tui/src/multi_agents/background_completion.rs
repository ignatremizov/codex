use codex_app_server_protocol::CollabAgentState;
use codex_app_server_protocol::CollabAgentStatus;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::SubAgentCompletionStatus;
use codex_protocol::protocol::sub_agent_completion_status_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_transcript_parts;
use ratatui::style::Stylize;
use ratatui::text::Span;

use super::AgentMetadata;
use super::CollabAgentHistoryCell;
use super::agent_label;
use super::agent_label_spans;
use super::collab_event;
use super::completion_agent_lines;
use super::parse_thread_id;
use super::title_text;

pub(crate) fn background_completion_history_cell_from_agent_message(
    id: &str,
    text: &str,
    phase: Option<&MessagePhase>,
    agent_response_preview_lines: usize,
    mut agent_metadata: impl FnMut(codex_protocol::ThreadId) -> AgentMetadata,
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
    let label = if let Some(thread_id) = parse_thread_id(agent_reference) {
        let metadata = agent_metadata(thread_id);
        agent_label_spans(agent_label(thread_id, &metadata))
    } else if agent_reference.is_empty() {
        vec![Span::from("agent").cyan()]
    } else {
        vec![Span::from(agent_reference.to_string()).cyan()]
    };
    let status = CollabAgentState { status, message };
    Some(collab_event(
        title_text("Agent finished"),
        completion_agent_lines(label, &status, agent_response_preview_lines),
    ))
}
