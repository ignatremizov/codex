//! One lifecycle row group per array-target send, with the shared message shown once.

use super::*;
use codex_protocol::CollabAgentInputBatch;
use codex_protocol::CollabAgentInputStatus;

pub(super) fn history_cell(
    batch: &CollabAgentInputBatch,
    status: &CollabAgentToolCallStatus,
    prompt: &str,
    observation: V1ResponseObservation,
    preview_lines: usize,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> Option<CollabAgentHistoryCell> {
    let title = match status {
        CollabAgentToolCallStatus::InProgress => return None,
        CollabAgentToolCallStatus::Completed => "Input batch completed",
        CollabAgentToolCallStatus::Failed => "Input batch completed with errors",
        CollabAgentToolCallStatus::Interrupted => {
            "Input batch interrupted; some input may have been accepted"
        }
    };
    let mut rows = Vec::new();
    for result in &batch.results {
        let prefix = match result.status {
            CollabAgentInputStatus::Submitted => "Sent input to",
            CollabAgentInputStatus::Queued => "Queued input for",
            CollabAgentInputStatus::MailboxAccepted => "Saved to mailbox for",
            CollabAgentInputStatus::Error => "Send error for",
        };
        let mut spans = vec![format!("{prefix} ").bold()];
        if let Some(receiver) = result
            .receiver_thread_id
            .as_deref()
            .and_then(parse_thread_id)
        {
            spans.extend(agent_label_spans(agent_label(
                receiver,
                &agent_metadata(receiver),
            )));
        } else {
            spans.push(result.target.escape_debug().to_string().cyan());
        }
        let row = if batch.flags == "z" || batch.flags == "zf" {
            spans.push(if batch.flags == "zf" {
                " (mailbox · conditional final wake)".magenta()
            } else {
                " (mailbox)".dim()
            });
            Line::from(spans)
        } else {
            append_response_observation_to_title(spans.into(), observation)
        };
        rows.push(row);
        if let Some(error) = &result.error {
            rows.push(
                truncate_text(error, COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES)
                    .red()
                    .into(),
            );
        }
        if let Some(hint) = &result.hint {
            rows.push(hint.clone().dim().into());
        }
    }
    let mut details = fixed_details(rows);
    details.extend(prompt_lines(prompt, preview_lines));
    Some(collab_event(title_text(title), details))
}

#[cfg(test)]
#[path = "send_input_batch_tests.rs"]
mod tests;
