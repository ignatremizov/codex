//! Render trusted background completions with the shared collaboration preview owner.

use codex_protocol::AgentPath;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::SubAgentCompletionStatus;
use codex_protocol::protocol::agent_delivery_receipt_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_model_visibility_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_status_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_transcript_parts;
use ratatui::style::Stylize;
use ratatui::text::Span;

use super::AgentMetadata;
use super::CollabAgentHistoryCell;
use super::CollabDetail;
use super::collab_event;
use super::parse_thread_id;
use super::preview_source_lines;
use super::title_spans_line;

/// Decodes the reserved identity emitted by the trusted core-to-app-server projection.
/// Raw core items must pass `has_sub_agent_completion_identity` before using this renderer.
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
    if let Some((sender, recipient, delivered_phase, recipient_model_visibility)) =
        agent_delivery_receipt_from_response_item_id(id)
    {
        let label = match delivered_phase {
            MessagePhase::Commentary => " · commentary delivered (",
            MessagePhase::FinalAnswer => " · final delivered (",
        };
        let mut sender_metadata = agent_metadata(sender);
        // These raw root copies are emitted only for acknowledged delivery of Main's output.
        sender_metadata.agent_nickname = Some("Main".to_string());
        let agent_title = super::CollabAgentTitle {
            thread_id: sender,
            metadata: sender_metadata,
            recipient: Some((recipient, agent_metadata(recipient))),
            recipient_separator: " → ",
            suffix: vec![
                label.dim(),
                completion_visibility_span(recipient_model_visibility),
                " to recipient):".dim(),
            ],
        };
        return Some(CollabAgentHistoryCell {
            title: agent_title.render(),
            agent_title: Some(agent_title),
            details: vec![CollabDetail::preview(
                preview_source_lines(text),
                agent_response_preview_lines,
            )],
        });
    }
    let completion_status = sub_agent_completion_status_from_response_item_id(id)?;
    let model_visibility = sub_agent_completion_model_visibility_from_response_item_id(id)?;
    let (agent_reference, payload) = sub_agent_completion_transcript_parts(text)?;
    let message = match completion_status {
        SubAgentCompletionStatus::Completed | SubAgentCompletionStatus::Errored => Some(payload),
        SubAgentCompletionStatus::Shutdown | SubAgentCompletionStatus::NotFound => None,
    };
    let agent_reference = agent_reference.trim();
    let thread_id = parse_thread_id(agent_reference);
    let details = message
        .map(preview_source_lines)
        .filter(|lines| !lines.is_empty())
        .map(|lines| vec![CollabDetail::preview(lines, agent_response_preview_lines)])
        .unwrap_or_default();
    let suffix = vec![
        Span::from(" ").dim(),
        completion_status_verb(completion_status),
        Span::from(" (").bold(),
        completion_visibility_span(model_visibility),
        Span::from(if details.is_empty() { ")" } else { "):" }).bold(),
    ];
    Some(if let Some(thread_id) = thread_id {
        CollabAgentHistoryCell::new_agent_labeled(
            thread_id,
            &agent_metadata(thread_id),
            suffix,
            details,
        )
    } else {
        let mut title = if agent_reference == AgentPath::ROOT {
            vec![
                Span::from("Main").cyan().bold(),
                Span::from(" ").dim(),
                Span::from("[default]"),
            ]
        } else if agent_reference.is_empty() {
            vec![Span::from("agent").cyan()]
        } else {
            vec![Span::from(agent_reference.to_string()).cyan()]
        };
        title.extend(suffix);
        collab_event(title_spans_line(title), details)
    })
}

#[cfg(test)]
#[path = "background_completion_tests.rs"]
mod tests;

fn completion_visibility_span(
    model_visibility: SubAgentCompletionModelVisibility,
) -> Span<'static> {
    match model_visibility {
        SubAgentCompletionModelVisibility::Visible => "● visible".green().bold(),
        SubAgentCompletionModelVisibility::NotVisible => "○ not visible".cyan().bold(),
    }
}

fn completion_status_verb(status: SubAgentCompletionStatus) -> Span<'static> {
    match status {
        SubAgentCompletionStatus::Completed => "completed".green(),
        SubAgentCompletionStatus::Errored => "errored".red(),
        SubAgentCompletionStatus::Shutdown => "shut down".into(),
        SubAgentCompletionStatus::NotFound => "not found".red(),
    }
}
