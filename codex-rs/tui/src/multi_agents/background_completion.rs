use codex_app_server_protocol::CollabAgentStatus;
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
    let thread_id = parse_thread_id(agent_reference);
    let details = message
        .as_deref()
        .map(preview_source_lines)
        .filter(|lines| !lines.is_empty())
        .map(|lines| vec![CollabDetail::preview(lines, agent_response_preview_lines)])
        .unwrap_or_default();
    let suffix = vec![
        Span::from(" ").dim(),
        completion_status_verb(&status),
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

fn completion_status_verb(status: &CollabAgentStatus) -> Span<'static> {
    match status {
        CollabAgentStatus::PendingInit => "pending initialization".cyan(),
        CollabAgentStatus::Running => "running".cyan().bold(),
        // Allow `.yellow()`
        #[allow(clippy::disallowed_methods)]
        CollabAgentStatus::Interrupted => "interrupted".yellow(),
        CollabAgentStatus::Completed => "completed".green(),
        CollabAgentStatus::Errored => "errored".red(),
        CollabAgentStatus::Shutdown => "shut down".into(),
        CollabAgentStatus::NotFound => "not found".red(),
    }
}
