use super::*;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::MailboxReadSelector;
use pretty_assertions::assert_eq;

#[test]
fn mailbox_read_renders_selected_sender_and_actual_outcome() {
    let sender = ThreadId::new();
    let item = MailboxReadItem {
        id: "check-mail-1".into(),
        selector: MailboxReadSelector::Agent {
            thread_id: sender.to_string(),
        },
        consumed_count: 2,
        rejected_count: 1,
    };
    let cell = history_cell_for_mailbox_read(&item, |_| AgentMetadata {
        agent_ref: Some("17".into()),
        agent_nickname: Some("Darwin".into()),
        ..Default::default()
    });
    insta::assert_snapshot!(
        cell.display_lines(/*width*/ 100)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        @"
    • Checked mailbox from Darwin (17) · 2 messages consumed · 1 message rejected
    "
    );
}

#[test]
fn mailbox_read_empty_selection_is_distinct_from_consumption() {
    let item = MailboxReadItem {
        id: "check-mail-empty".into(),
        selector: MailboxReadSelector::All,
        consumed_count: 0,
        rejected_count: 0,
    };
    let cell = history_cell_for_mailbox_read(&item, |_| AgentMetadata::default());

    assert_eq!(
        cell.display_lines(/*width*/ 100)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["• Checked all pending mail · empty"]
    );
}
