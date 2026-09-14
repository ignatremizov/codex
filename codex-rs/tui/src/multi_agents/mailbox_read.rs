//! Durable, payload-free presentation of a completed direct `check_mail` call.

use super::AgentMetadata;
use super::CollabAgentHistoryCell;
use super::agent_label;
use super::collab_event;
use super::parse_thread_id;
use super::title_text;
use super::title_with_agent;
use codex_app_server_protocol::MailboxReadItem;
use codex_app_server_protocol::MailboxReadSelector;
use codex_protocol::ThreadId;
use ratatui::style::Stylize;

pub(crate) fn history_cell_for_mailbox_read(
    item: &MailboxReadItem,
    mut agent_metadata: impl FnMut(ThreadId) -> AgentMetadata,
) -> CollabAgentHistoryCell {
    let mut title = match &item.selector {
        MailboxReadSelector::All => title_text("Checked all pending mail"),
        MailboxReadSelector::User => title_text("Checked mailbox from user"),
        MailboxReadSelector::Agent { thread_id } => match parse_thread_id(thread_id) {
            Some(thread_id) => title_with_agent(
                "Checked mailbox from",
                agent_label(thread_id, &agent_metadata(thread_id)),
                /*spawn_request*/ None,
            ),
            None => title_text("Checked mailbox for an unknown agent"),
        },
    };

    match (item.consumed_count, item.rejected_count) {
        (0, 0) => title.spans.push(" · empty".dim()),
        (consumed_count, rejected_count) => {
            title.spans.push(" · ".dim());
            if consumed_count > 0 {
                title
                    .spans
                    .push(format!("{} consumed", message_count(consumed_count)).green());
            }
            if consumed_count > 0 && rejected_count > 0 {
                title.spans.push(" · ".dim());
            }
            if rejected_count > 0 {
                title
                    .spans
                    .push(format!("{} rejected", message_count(rejected_count)).red());
            }
        }
    }

    collab_event(title, Vec::new())
}

fn message_count(count: u64) -> String {
    format!(
        "{count} {}",
        if count == 1 { "message" } else { "messages" }
    )
}

#[cfg(test)]
#[path = "mailbox_read_tests.rs"]
mod tests;
