//! Mailbox acceptance is distinct from immediate input and response subscriptions.

use super::AgentMetadata;
use super::CollabAgentHistoryCell;
use super::agent_label;
use super::collab_event;
use super::prompt_lines;
use super::title_with_agent;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_protocol::ThreadId;

pub(super) fn history_cell(
    receiver: ThreadId,
    status: CollabAgentToolCallStatus,
    prompt: &str,
    preview_lines: usize,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> CollabAgentHistoryCell {
    let prefix = match status {
        CollabAgentToolCallStatus::InProgress => "Saving to mailbox for",
        CollabAgentToolCallStatus::Completed => "Saved to mailbox for",
        CollabAgentToolCallStatus::Failed => "Failed to save to mailbox for",
        CollabAgentToolCallStatus::Interrupted => "Interrupted mailbox send to",
    };
    // Acceptance does not establish that the receiver consumed the message, and
    // the ordinary c/f/x labels describe subscriptions that a mailbox send never creates.
    let title = title_with_agent(
        prefix,
        agent_label(receiver, &agent_metadata(receiver)),
        /*spawn_request*/ None,
    );
    collab_event(title, prompt_lines(prompt, preview_lines))
}

#[cfg(test)]
#[path = "mailbox_send_tests.rs"]
mod tests;
