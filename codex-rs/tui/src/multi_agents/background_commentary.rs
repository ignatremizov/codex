use codex_app_server_protocol::attributed_agent_message_transcript_parts;
use codex_app_server_protocol::sub_agent_commentary_transcript_parts;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::agent_message_audit_transcript_parts;
use codex_protocol::protocol::is_attributed_agent_message_response_item_id;
use ratatui::style::Stylize;

use super::AgentMetadata;
use super::CollabAgentHistoryCell;
use super::CollabDetail;
use super::parse_thread_id;
use super::preview_source_lines;

pub(crate) fn background_commentary_history_cell_from_agent_message(
    id: &str,
    text: &str,
    phase: Option<&MessagePhase>,
    agent_response_preview_lines: usize,
    mut agent_metadata: impl FnMut(codex_protocol::ThreadId) -> AgentMetadata,
) -> Option<CollabAgentHistoryCell> {
    if phase != Some(&MessagePhase::Commentary) {
        return None;
    }
    if is_attributed_agent_message_response_item_id(id)
        && let Some((sender, recipient, message)) = agent_message_audit_transcript_parts(text)
    {
        let agent_title = super::CollabAgentTitle {
            thread_id: sender,
            metadata: agent_metadata(sender),
            recipient: Some((recipient, agent_metadata(recipient))),
            recipient_separator: " sends to ",
            suffix: vec![" (".bold(), "○ not visible".cyan().bold(), "):".bold()],
        };
        return Some(CollabAgentHistoryCell {
            title: agent_title.render(),
            agent_title: Some(agent_title),
            details: vec![CollabDetail::preview(
                preview_source_lines(message),
                agent_response_preview_lines,
            )],
        });
    }
    let (agent_reference, message) = sub_agent_commentary_transcript_parts(text).or_else(|| {
        is_attributed_agent_message_response_item_id(id)
            .then(|| attributed_agent_message_transcript_parts(text))
            .flatten()
    })?;
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

#[cfg(test)]
#[path = "background_commentary_tests.rs"]
mod tests;
