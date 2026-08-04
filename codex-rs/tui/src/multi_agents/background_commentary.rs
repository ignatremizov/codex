use codex_app_server_protocol::sub_agent_commentary_transcript_parts;
use codex_protocol::models::MessagePhase;
use ratatui::style::Stylize;

use super::AgentMetadata;
use super::CollabAgentHistoryCell;
use super::CollabDetail;
use super::parse_thread_id;
use super::preview_source_lines;

pub(crate) fn background_commentary_history_cell_from_agent_message(
    text: &str,
    phase: Option<&MessagePhase>,
    agent_response_preview_lines: usize,
    mut agent_metadata: impl FnMut(codex_protocol::ThreadId) -> AgentMetadata,
) -> Option<CollabAgentHistoryCell> {
    if phase != Some(&MessagePhase::Commentary) {
        return None;
    }
    let (agent_reference, message) = sub_agent_commentary_transcript_parts(text)?;
    let thread_id = parse_thread_id(agent_reference.trim())?;
    let details = vec![CollabDetail::preview(
        preview_source_lines(message),
        agent_response_preview_lines,
    )];
    Some(CollabAgentHistoryCell::new_agent_labeled(
        thread_id,
        &agent_metadata(thread_id),
        vec![" sends:".bold()],
        details,
    ))
}
