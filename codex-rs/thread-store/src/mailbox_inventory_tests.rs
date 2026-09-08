use super::stores;
use super::user_submission;
use crate::AppendThreadItemsParams;
use crate::CreateThreadParams;
use crate::MailboxInventory;
use crate::MailboxInventoryAcknowledgement;
use crate::MailboxInventoryAcknowledgementOutcome;
use crate::MailboxInventoryRecovery;
use crate::MailboxMessageState;
use crate::MailboxSender;
use crate::MailboxSenderInventory;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

async fn create_thread(store: &dyn ThreadStore, receiver: ThreadId, mode: ThreadHistoryMode) {
    store
        .create_thread(CreateThreadParams {
            session_id: receiver.into(),
            thread_id: receiver,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "inventory-test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: mode,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: ThreadId::new().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(std::env::current_dir().unwrap()),
                model_provider: "test-provider".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn inventory_proof_and_retired_retry_preserve_newer_active_notification() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            let receiver = ThreadId::new();
            create_thread(store.as_ref(), receiver, mode).await;
            assert_eq!(
                store.read_mailbox_inventory(receiver).await.unwrap(),
                MailboxInventory {
                    receiver_thread_id: receiver,
                    notified_through: 0,
                    pending_senders: Vec::new(),
                    claimed_senders: Vec::new(),
                    active_notification: None,
                }
            );
            assert_eq!(
                store.prepare_mailbox_inventory(receiver).await.unwrap(),
                None
            );
            let accepted = store
                .accept_mailbox_input(user_submission(receiver, "private-payload"))
                .await
                .unwrap();
            let original = store
                .prepare_mailbox_inventory(receiver)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                original.pending_senders,
                vec![MailboxSenderInventory {
                    sender: MailboxSender::User,
                    count: 1,
                    max_acceptance_sequence: accepted.acceptance_sequence,
                }]
            );
            let before = store.read_mailbox_inventory(receiver).await.unwrap();
            let mut changed_snapshot = original.clone();
            changed_snapshot.pending_senders[0].count += 1;
            assert!(
                store
                    .recover_mailbox_inventory(changed_snapshot.clone())
                    .await
                    .is_err()
            );
            assert!(
                store
                    .cancel_mailbox_inventory(changed_snapshot)
                    .await
                    .is_err()
            );
            assert_eq!(
                store
                    .recover_mailbox_inventory(original.clone())
                    .await
                    .unwrap(),
                MailboxInventoryRecovery::NotRecorded
            );
            assert!(
                store
                    .reconcile_mailbox_inventory(original.clone())
                    .await
                    .is_err()
            );
            assert_eq!(
                store.read_mailbox_inventory(receiver).await.unwrap(),
                before
            );
            let mut context = original.context().unwrap();
            context
                .item
                .set_create_time_if_missing(serde_json::Number::from(12345));
            let text = serde_json::to_string(&context.item).unwrap();
            assert!(!text.contains("private-payload"));
            assert!(!text.contains("image_url"));
            store
                .append_items_and_flush(AppendThreadItemsParams {
                    thread_id: receiver,
                    items: vec![RolloutItem::ResponseItem(context.clone())],
                })
                .await
                .unwrap();
            assert_eq!(
                store
                    .recover_mailbox_inventory(original.clone())
                    .await
                    .unwrap(),
                MailboxInventoryRecovery::Recorded {
                    context: context.clone()
                }
            );
            assert!(
                store
                    .cancel_mailbox_inventory(original.clone())
                    .await
                    .is_err()
            );
            // Drop the first successful receipt, modelling a lost result/read failure.
            store
                .reconcile_mailbox_inventory(original.clone())
                .await
                .unwrap();
            assert_eq!(
                store.prepare_mailbox_inventory(receiver).await.unwrap(),
                None
            );
            assert_eq!(
                store
                    .lookup_mailbox_input(receiver, "private-payload")
                    .await
                    .unwrap(),
                Some(accepted)
            );
            store
                .accept_mailbox_input(user_submission(receiver, "later"))
                .await
                .unwrap();
            let newer = store
                .prepare_mailbox_inventory(receiver)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(newer.id, original.id);
            assert_eq!(newer.pending_senders[0].count, 2);
            let newer_state = store.read_mailbox_inventory(receiver).await.unwrap();
            assert_eq!(
                store
                    .reconcile_mailbox_inventory(original.clone())
                    .await
                    .unwrap(),
                MailboxInventoryAcknowledgement {
                    notification_id: original.id.clone(),
                    notified_through: original.through_sequence,
                    outcome: MailboxInventoryAcknowledgementOutcome::AlreadyCovered,
                }
            );
            assert_eq!(
                store
                    .recover_mailbox_inventory(original.clone())
                    .await
                    .unwrap(),
                MailboxInventoryRecovery::AlreadyCovered {
                    context,
                    notified_through: original.through_sequence,
                }
            );
            assert_eq!(
                store.read_mailbox_inventory(receiver).await.unwrap(),
                newer_state
            );
            let mut invented = original.clone();
            invented.id = ThreadId::new().to_string();
            assert!(store.reconcile_mailbox_inventory(invented).await.is_err());
            let mut changed_original = original.clone();
            changed_original.pending_senders[0].count += 1;
            assert!(
                store
                    .reconcile_mailbox_inventory(changed_original)
                    .await
                    .is_err()
            );
            assert_eq!(
                store.read_mailbox_inventory(receiver).await.unwrap(),
                newer_state
            );
            assert!(store.cancel_mailbox_inventory(original).await.is_err());
            store.cancel_mailbox_inventory(newer.clone()).await.unwrap();
            let cancelled = store.read_mailbox_inventory(receiver).await.unwrap();
            assert_eq!(cancelled.notified_through, newer_state.notified_through);
            assert_eq!(cancelled.pending_senders, newer_state.pending_senders);
            assert_eq!(cancelled.active_notification, None);
            let replacement = store
                .prepare_mailbox_inventory(receiver)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(replacement.id, newer.id);
            let replacement_state = store.read_mailbox_inventory(receiver).await.unwrap();
            assert!(store.cancel_mailbox_inventory(newer).await.is_err());
            assert_eq!(
                store.read_mailbox_inventory(receiver).await.unwrap(),
                replacement_state
            );
            store.shutdown_thread(receiver).await.unwrap();
        }
    }
}

#[tokio::test]
async fn inventory_conflicts_never_acknowledge_or_cancel() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        for change in ["role", "turn", "groups", "receiver", "duplicate-stamp"] {
            let receiver = ThreadId::new();
            create_thread(store.as_ref(), receiver, ThreadHistoryMode::Legacy).await;
            store
                .accept_mailbox_input(user_submission(receiver, "pending"))
                .await
                .unwrap();
            let notification = store
                .prepare_mailbox_inventory(receiver)
                .await
                .unwrap()
                .unwrap();
            let before = store.read_mailbox_inventory(receiver).await.unwrap();
            let original = notification.context().unwrap();
            let mut changed = original.clone();
            if change == "receiver" {
                let mut foreign = notification.clone();
                foreign.receiver_thread_id = ThreadId::new();
                changed = foreign.context().unwrap();
            }
            let ResponseItem::Message {
                role,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } = &mut changed.item
            else {
                panic!("expected inventory context");
            };
            match change {
                "role" => *role = "user".to_string(),
                "turn" => {
                    internal_chat_message_metadata_passthrough
                        .as_mut()
                        .unwrap()
                        .turn_id = Some("foreign-turn".to_string())
                }
                "groups" => content.push(ContentItem::InputText {
                    text: "different counts".to_string(),
                }),
                "receiver" | "duplicate-stamp" => {}
                _ => unreachable!(),
            }
            let mut items = Vec::new();
            if change == "duplicate-stamp" {
                items.push(RolloutItem::ResponseItem(original));
                changed
                    .item
                    .set_create_time_if_missing(serde_json::Number::from(987));
            }
            items.push(RolloutItem::ResponseItem(changed));
            store
                .append_items_and_flush(AppendThreadItemsParams {
                    thread_id: receiver,
                    items,
                })
                .await
                .unwrap();
            assert!(
                store
                    .recover_mailbox_inventory(notification.clone())
                    .await
                    .is_err(),
                "{change}"
            );
            assert!(
                store
                    .reconcile_mailbox_inventory(notification.clone())
                    .await
                    .is_err(),
                "{change}"
            );
            assert!(
                store.cancel_mailbox_inventory(notification).await.is_err(),
                "{change}"
            );
            assert_eq!(
                store.read_mailbox_inventory(receiver).await.unwrap(),
                before
            );
            store.shutdown_thread(receiver).await.unwrap();
        }
    }
}

#[tokio::test]
async fn inventory_preparation_stays_fixed_when_pending_mail_is_claimed_or_new_mail_arrives() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let accepted = store
            .accept_mailbox_input(user_submission(receiver, "first"))
            .await
            .unwrap();
        let original = store
            .prepare_mailbox_inventory(receiver)
            .await
            .unwrap()
            .unwrap();
        store
            .claim_mailbox_input(super::claim_params(receiver, "check"))
            .await
            .unwrap();
        let inventory = store.read_mailbox_inventory(receiver).await.unwrap();
        assert!(inventory.pending_senders.is_empty());
        assert_eq!(inventory.claimed_senders, original.pending_senders);
        assert_eq!(
            store.prepare_mailbox_inventory(receiver).await.unwrap(),
            Some(original.clone())
        );
        store
            .accept_mailbox_input(user_submission(receiver, "new"))
            .await
            .unwrap();
        assert_eq!(
            store.prepare_mailbox_inventory(receiver).await.unwrap(),
            Some(original)
        );
        let mut claimed = accepted;
        claimed.state = MailboxMessageState::Claimed;
        assert_eq!(
            store.lookup_mailbox_input(receiver, "first").await.unwrap(),
            Some(claimed)
        );
    }
}

#[tokio::test]
async fn inventory_recovery_survives_reopening_with_original_snapshot_and_exact_stamps() {
    let home = tempfile::tempdir().unwrap();
    let (original_stores, runtime) = stores(&home).await;
    let store = &original_stores[1];
    let receiver = ThreadId::new();
    create_thread(store.as_ref(), receiver, ThreadHistoryMode::Legacy).await;
    store
        .accept_mailbox_input(user_submission(receiver, "pending"))
        .await
        .unwrap();
    let original = store
        .prepare_mailbox_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    let mut context = original.context().unwrap();
    context
        .item
        .set_create_time_if_missing(serde_json::Number::from(567));
    store
        .append_items_and_flush(AppendThreadItemsParams {
            thread_id: receiver,
            items: vec![RolloutItem::ResponseItem(context.clone())],
        })
        .await
        .unwrap();
    store
        .reconcile_mailbox_inventory(original.clone())
        .await
        .unwrap();
    store
        .accept_mailbox_input(user_submission(receiver, "later"))
        .await
        .unwrap();
    let newer = store.prepare_mailbox_inventory(receiver).await.unwrap();
    store.shutdown_thread(receiver).await.unwrap();
    drop(original_stores);
    drop(runtime);
    let (reopened, _) = stores(&home).await;
    assert_eq!(
        reopened[1]
            .recover_mailbox_inventory(original.clone())
            .await
            .unwrap(),
        MailboxInventoryRecovery::AlreadyCovered {
            context,
            notified_through: original.through_sequence
        }
    );
    assert_eq!(
        reopened[1]
            .reconcile_mailbox_inventory(original)
            .await
            .unwrap()
            .outcome,
        MailboxInventoryAcknowledgementOutcome::AlreadyCovered
    );
    assert_eq!(
        reopened[1]
            .read_mailbox_inventory(receiver)
            .await
            .unwrap()
            .active_notification,
        newer
    );
}

#[tokio::test]
async fn inventory_groups_canonical_authors_without_payload_or_volatile_labels() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let sender = ThreadId::new();
        let user = store
            .accept_mailbox_input(user_submission(receiver, "user"))
            .await
            .unwrap();
        let agent = store
            .accept_mailbox_input(crate::AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: "agent".to_string(),
                payload: crate::MailboxPayload::Agent {
                    input: vec![codex_protocol::user_input::UserInput::Text {
                        text: "secret agent payload".to_string(),
                        text_elements: Vec::new(),
                    }],
                    attribution: Box::new(codex_protocol::AgentInputAttribution {
                        sender: super::identity(sender),
                        recipient: super::identity(receiver),
                        sender_turn_id: "sender-turn".to_string(),
                    }),
                },
            })
            .await
            .unwrap();
        let notification = store
            .prepare_mailbox_inventory(receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            notification.pending_senders,
            vec![
                MailboxSenderInventory {
                    sender: MailboxSender::Agent(sender),
                    count: 1,
                    max_acceptance_sequence: agent.acceptance_sequence
                },
                MailboxSenderInventory {
                    sender: MailboxSender::User,
                    count: 1,
                    max_acceptance_sequence: user.acceptance_sequence
                },
            ]
        );
        let context = notification.context().unwrap();
        let serialized = serde_json::to_string(&context.item).unwrap();
        assert!(serialized.contains(&sender.to_string()));
        assert!(!serialized.contains("snapshot-name"));
        assert!(!serialized.contains("secret agent payload"));
        assert!(
            store
                .cancel_mailbox_inventory(notification.clone())
                .await
                .is_err()
        );
        assert!(
            store
                .reconcile_mailbox_inventory(notification.clone())
                .await
                .is_err()
        );
        assert_eq!(
            store.prepare_mailbox_inventory(receiver).await.unwrap(),
            Some(notification)
        );
    }
}
