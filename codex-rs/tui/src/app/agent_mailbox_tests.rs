use codex_app_server_protocol::AgentAlias;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn mailbox_details_distinguish_loading_unavailable_and_empty_snapshots() {
    let render = |details: AgentMailboxDetails| {
        details
            .lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };

    assert_eq!(
        render(AgentMailboxDetails::Loading),
        "Pending mail: loading"
    );
    assert_eq!(
        render(AgentMailboxDetails::Unavailable),
        "Pending mail: unavailable"
    );
    assert_eq!(
        render(AgentMailboxDetails::Inventory {
            pending_total: 0,
            sender_counts: Vec::new(),
        }),
        "Pending mail: 0"
    );
    insta::assert_snapshot!(
        render(AgentMailboxDetails::Inventory {
            pending_total: 3,
            sender_counts: vec![("ref 2".to_string(), 2), ("user".to_string(), 1)],
        }),
        @r"
    Pending mail: 3
      ref 2: 2
      user: 1
    "
    );
}

#[test]
fn mailbox_counts_use_receiver_scope_refs_and_uuid_fallbacks() {
    let receiver = codex_protocol::ThreadId::from_u128(1);
    let aliased_sender = codex_protocol::ThreadId::from_u128(2);
    let transferred_sender = codex_protocol::ThreadId::from_u128(3);
    let unknown_sender = codex_protocol::ThreadId::from_u128(4);
    let mut navigation = AgentNavigationState::default();
    navigation.replace_aliases(vec![
        AgentAlias {
            task_path: None,
            thread_id: aliased_sender.to_string(),
            agent_ref: "2".to_string(),
            nickname: None,
            state: AgentAliasState::Active,
        },
        AgentAlias {
            task_path: None,
            thread_id: transferred_sender.to_string(),
            agent_ref: "3".to_string(),
            nickname: None,
            state: AgentAliasState::Transferred,
        },
    ]);

    assert_eq!(
        mailbox_inventory_display(
            receiver,
            &navigation,
            Ok(ThreadMailboxReadResponse {
                pending_total: 6,
                pending_senders: vec![
                    ThreadMailboxPendingSender::Agent {
                        thread_id: aliased_sender.to_string(),
                        count: 2,
                    },
                    ThreadMailboxPendingSender::Agent {
                        thread_id: transferred_sender.to_string(),
                        count: 1,
                    },
                    ThreadMailboxPendingSender::Agent {
                        thread_id: unknown_sender.to_string(),
                        count: 1,
                    },
                    ThreadMailboxPendingSender::Agent {
                        thread_id: "invalid-thread-id".to_string(),
                        count: 1,
                    },
                    ThreadMailboxPendingSender::User { count: 1 },
                ],
            }),
        ),
        AgentMailboxInventoryDisplay::Available {
            pending_total: 6,
            sender_counts: vec![
                ("ref 2".to_string(), 2),
                (transferred_sender.to_string(), 1),
                (unknown_sender.to_string(), 1),
                ("invalid-thread-id".to_string(), 1),
                ("user".to_string(), 1),
            ],
        },
    );
}
