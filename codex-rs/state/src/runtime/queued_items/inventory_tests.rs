use super::*;
use crate::MailboxClaim;
use crate::MailboxClaimedMessage;
use crate::MailboxFinalSubscriptionRequest;
use crate::MailboxFinalSubscriptionState;
use crate::MailboxInvocation;
use crate::MailboxMessage;
use crate::MailboxMessageState;
use crate::MailboxSelection;
use crate::StateRuntime;
use crate::migrations::QUEUE_MIGRATOR;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrator;
use std::borrow::Cow;
use std::sync::Arc;

async fn runtime() -> Arc<StateRuntime> {
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(unique_temp_dir().as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap()
}

fn assert_stale(error: anyhow::Error) {
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn fixed_inventory_and_watermark_survive_reopen_without_repeat_eligibility() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    assert_eq!(
        queue.read_mail_inventory(receiver).await.unwrap(),
        MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: 0,
            pending_senders: Vec::new(),
            claimed_senders: Vec::new(),
            active_notification: None,
        }
    );
    assert_eq!(queue.prepare_mail_inventory(receiver).await.unwrap(), None);
    let first = queue
        .accept_mail(receiver, "first", "user", "{}")
        .await
        .unwrap();
    let prepared = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    let original_group = MailboxSenderInventory {
        sender_key: "user".to_string(),
        count: 1,
        max_acceptance_sequence: first.acceptance_sequence,
    };
    assert_eq!(
        prepared,
        MailboxInventoryNotification {
            id: prepared.id.clone(),
            receiver_thread_id: receiver,
            through_sequence: first.acceptance_sequence,
            pending_senders: vec![original_group],
        }
    );
    let later = queue
        .accept_mail(receiver, "later", "user", "{}")
        .await
        .unwrap();
    assert_eq!(
        queue.prepare_mail_inventory(receiver).await.unwrap(),
        Some(prepared.clone())
    );
    let config = runtime.sqlite().clone();
    runtime.close().await;
    let reopened = StateRuntime::init(config.clone(), "test-provider".to_string())
        .await
        .unwrap();
    let queue = reopened.thread_queue();
    assert_eq!(
        queue.prepare_mail_inventory(receiver).await.unwrap(),
        Some(prepared.clone())
    );
    assert_eq!(
        queue
            .acknowledge_mail_inventory(receiver, &prepared.id)
            .await
            .unwrap(),
        first.acceptance_sequence
    );
    assert_stale(
        queue
            .acknowledge_mail_inventory(receiver, &prepared.id)
            .await
            .unwrap_err(),
    );
    let next = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next.id, prepared.id);
    assert_eq!(
        next,
        MailboxInventoryNotification {
            id: next.id.clone(),
            receiver_thread_id: receiver,
            through_sequence: later.acceptance_sequence,
            pending_senders: vec![MailboxSenderInventory {
                sender_key: "user".to_string(),
                count: 2,
                max_acceptance_sequence: later.acceptance_sequence,
            }],
        }
    );
    assert_stale(
        queue
            .acknowledge_mail_inventory(receiver, &prepared.id)
            .await
            .unwrap_err(),
    );
    assert_stale(
        queue
            .cancel_mail_inventory(receiver, &prepared.id)
            .await
            .unwrap_err(),
    );
    assert_eq!(
        queue.prepare_mail_inventory(receiver).await.unwrap(),
        Some(next.clone())
    );
    queue
        .acknowledge_mail_inventory(receiver, &next.id)
        .await
        .unwrap();
    assert_eq!(
        queue.read_mail(receiver, &first.id).await.unwrap(),
        Some(first)
    );
    assert_eq!(
        queue.read_mail(receiver, &later.id).await.unwrap(),
        Some(later.clone())
    );
    assert_eq!(queue.prepare_mail_inventory(receiver).await.unwrap(), None);
    reopened.close().await;
    let reopened = StateRuntime::init(config, "test-provider".to_string())
        .await
        .unwrap();
    let queue = reopened.thread_queue();
    assert_eq!(queue.prepare_mail_inventory(receiver).await.unwrap(), None);
    assert_eq!(
        queue.read_mail_inventory(receiver).await.unwrap(),
        MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: later.acceptance_sequence,
            pending_senders: next.pending_senders,
            claimed_senders: Vec::new(),
            active_notification: None,
        }
    );
}

#[tokio::test]
async fn inventory_ack_binds_only_accepted_subscriptions_inside_its_fixed_frontier() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let first_sender = ThreadId::new();
    let first = queue
        .accept_mail_with_final_subscription(
            receiver,
            "first",
            &format!("agent:{first_sender}"),
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    let original = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    let second_sender = ThreadId::new();
    let second = queue
        .accept_mail_with_final_subscription(
            receiver,
            "second",
            &format!("agent:{second_sender}"),
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();

    let original_ack = queue
        .acknowledge_mail_inventory_with_final_subscriptions(receiver, &original.id)
        .await
        .unwrap();
    let mut expected_first = first.final_subscription.unwrap();
    expected_first.state = MailboxFinalSubscriptionState::Bound;
    expected_first.bound_turn_id = Some(original.id.clone());
    assert_eq!(
        original_ack.bound_subscriptions,
        vec![expected_first.clone()]
    );
    assert_eq!(
        queue
            .read_active_mailbox_final_subscription(receiver, first_sender)
            .await
            .unwrap(),
        Some(expected_first.clone())
    );

    // A later accepted message creates a new frontier; the previously bound
    // unread message is not rebound to this different inventory turn.
    let later = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    assert!(later.through_sequence > original.through_sequence);
    let later_ack = queue
        .acknowledge_mail_inventory_with_final_subscriptions(receiver, &later.id)
        .await
        .unwrap();
    let mut expected_second = second.final_subscription.unwrap();
    expected_second.state = MailboxFinalSubscriptionState::Bound;
    expected_second.bound_turn_id = Some(later.id.clone());
    assert_eq!(later_ack.bound_subscriptions, vec![expected_second.clone()]);
    assert_eq!(
        queue
            .read_active_mailbox_final_subscription(receiver, first_sender)
            .await
            .unwrap(),
        Some(expected_first)
    );

    let config = runtime.sqlite().clone();
    runtime.close().await;
    let reopened = StateRuntime::init(config, "test-provider".to_string())
        .await
        .unwrap();
    let queue = reopened.thread_queue();
    assert_eq!(queue.prepare_mail_inventory(receiver).await.unwrap(), None);
    assert_eq!(
        queue
            .read_mail(receiver, &first.id)
            .await
            .unwrap()
            .unwrap()
            .final_subscription
            .unwrap()
            .bound_turn_id,
        Some(original.id)
    );
    assert_eq!(
        queue
            .read_mail(receiver, &second.id)
            .await
            .unwrap()
            .unwrap()
            .final_subscription
            .unwrap()
            .bound_turn_id,
        Some(later.id)
    );
}

#[tokio::test]
async fn cancelled_unrecorded_inventory_does_not_bind_final_subscription() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    let accepted = queue
        .accept_mail_with_final_subscription(
            receiver,
            "pending",
            &format!("agent:{sender}"),
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    let notification = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    queue
        .cancel_mail_inventory(receiver, &notification.id)
        .await
        .unwrap();
    assert_eq!(
        queue
            .read_active_mailbox_final_subscription(receiver, sender)
            .await
            .unwrap()
            .unwrap()
            .state,
        MailboxFinalSubscriptionState::Pending
    );
    assert_eq!(
        queue
            .read_mail(receiver, &accepted.id)
            .await
            .unwrap()
            .unwrap()
            .final_subscription
            .unwrap()
            .bound_turn_id,
        None
    );
}

#[tokio::test]
async fn claimed_recovery_is_separate_and_obsolete_snapshots_require_explicit_retirement() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let first = queue
        .accept_mail(receiver, "first", "a", "{}")
        .await
        .unwrap();
    let second = queue
        .accept_mail(receiver, "second", "b", "{}")
        .await
        .unwrap();
    let third = queue
        .accept_mail(receiver, "third", "a", "{}")
        .await
        .unwrap();
    let foreign = ThreadId::new();
    queue
        .accept_mail(foreign, "foreign", "user", "{}")
        .await
        .unwrap();
    let prepared = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    let first_sender = MailboxSenderInventory {
        sender_key: "a".to_string(),
        count: 2,
        max_acceptance_sequence: third.acceptance_sequence,
    };
    let second_sender = MailboxSenderInventory {
        sender_key: "b".to_string(),
        count: 1,
        max_acceptance_sequence: second.acceptance_sequence,
    };
    assert_eq!(
        prepared.pending_senders,
        vec![first_sender.clone(), second_sender.clone()]
    );
    let invocation = MailboxInvocation {
        receiver_thread_id: receiver,
        turn_id: "turn".to_string(),
        tool_call_id: "check".to_string(),
    };
    let claim = queue
        .claim_mail(
            &invocation,
            &MailboxSelection::Senders(vec!["a".to_string()]),
        )
        .await
        .unwrap();
    assert_eq!(
        queue.read_mail_inventory(receiver).await.unwrap(),
        MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: 0,
            pending_senders: vec![second_sender],
            claimed_senders: vec![first_sender],
            active_notification: Some(prepared.clone()),
        }
    );
    queue
        .reject_mail(receiver, &second.id, "revoked")
        .await
        .unwrap();
    // Even an obsolete snapshot survives for canonical-history reconciliation.
    assert_eq!(
        queue.prepare_mail_inventory(receiver).await.unwrap(),
        Some(prepared.clone())
    );
    assert_stale(
        queue
            .cancel_mail_inventory(foreign, &prepared.id)
            .await
            .unwrap_err(),
    );
    assert_stale(
        queue
            .acknowledge_mail_inventory(foreign, &prepared.id)
            .await
            .unwrap_err(),
    );
    queue
        .cancel_mail_inventory(receiver, &prepared.id)
        .await
        .unwrap();
    assert_eq!(queue.prepare_mail_inventory(receiver).await.unwrap(), None);
    assert_eq!(
        queue.list_unconsumed_mail_claims(receiver).await.unwrap(),
        vec![claim.clone()]
    );
    assert_eq!(
        queue
            .read_mail_inventory(receiver)
            .await
            .unwrap()
            .notified_through,
        0
    );
    queue
        .acknowledge_mail(&invocation, &first.id, &claim.messages[0].delivery_id)
        .await
        .unwrap();
    queue
        .reject_mail(receiver, &third.id, "revoked")
        .await
        .unwrap();
    assert_eq!(
        queue.read_mail_inventory(receiver).await.unwrap(),
        MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: 0,
            pending_senders: Vec::new(),
            claimed_senders: Vec::new(),
            active_notification: None,
        }
    );
    assert!(
        queue
            .prepare_mail_inventory(foreign)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn concurrent_preparers_share_one_identity_and_ack_cancel_cannot_retire_new_work() {
    let runtime = runtime().await;
    let other = StateRuntime::init(runtime.sqlite().clone(), "test-provider".to_string())
        .await
        .unwrap();
    let receiver = ThreadId::new();
    let queue = runtime.thread_queue();
    let other_queue = other.thread_queue();
    queue
        .accept_mail(receiver, "first", "user", "{}")
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        queue.prepare_mail_inventory(receiver),
        other_queue.prepare_mail_inventory(receiver)
    );
    let prepared = first.unwrap().unwrap();
    assert_eq!(second.unwrap(), Some(prepared.clone()));
    let (ack, cancel) = tokio::join!(
        queue.acknowledge_mail_inventory(receiver, &prepared.id),
        other_queue.cancel_mail_inventory(receiver, &prepared.id)
    );
    let watermark = match (ack, cancel) {
        (Ok(watermark), Err(error)) => {
            assert_stale(error);
            watermark
        }
        (Err(error), Ok(())) => {
            assert_stale(error);
            0
        }
        outcomes => panic!("exactly one retirement must succeed: {outcomes:?}"),
    };
    let later = queue
        .accept_mail(receiver, "later", "user", "{}")
        .await
        .unwrap();
    let next = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next.id, prepared.id);
    assert_eq!(next.through_sequence, later.acceptance_sequence);
    assert_stale(
        other_queue
            .acknowledge_mail_inventory(receiver, &prepared.id)
            .await
            .unwrap_err(),
    );
    assert_eq!(
        queue.read_mail_inventory(receiver).await.unwrap(),
        MailboxInventory {
            receiver_thread_id: receiver,
            notified_through: watermark,
            pending_senders: next.pending_senders.clone(),
            claimed_senders: Vec::new(),
            active_notification: Some(next.clone()),
        }
    );
    // Cancellation retains eligibility and allocates a different identity on reprepare.
    queue
        .cancel_mail_inventory(receiver, &next.id)
        .await
        .unwrap();
    let replacement = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.id, next.id);
    assert_eq!(replacement.pending_senders, next.pending_senders);
    assert_eq!(replacement.through_sequence, next.through_sequence);
}

#[tokio::test]
async fn inventory_migration_preserves_existing_mail_claims_and_ordinary_queue() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home).await.unwrap();
    let config = crate::SqliteConfig::new_for_testing(home.as_path().abs());
    let pool = Arc::new(
        config
            .open_read_write_pool(&config.queue_db_path())
            .await
            .unwrap(),
    );
    let old_migrator = Migrator {
        migrations: Cow::Owned(
            QUEUE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= 3)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
        table_name: QUEUE_MIGRATOR.table_name.clone(),
        create_schemas: QUEUE_MIGRATOR.create_schemas.clone(),
    };
    old_migrator.run(pool.as_ref()).await.unwrap();
    let queue = SqliteQueueStore::new(Arc::clone(&pool));
    let receiver = ThreadId::new();
    let ordinary = queue
        .enqueue(receiver, r#"{"ordinary":true}"#)
        .await
        .unwrap();
    let invocation = MailboxInvocation {
        receiver_thread_id: receiver,
        turn_id: "existing-turn".to_string(),
        tool_call_id: "existing-call".to_string(),
    };
    let claimed_message_id = "legacy-claimed-message";
    let pending_message_id = "legacy-pending-message";
    let delivery_id = "legacy-delivery";

    // The v3 schema predates mailbox_final_subscriptions, which current queue methods join.
    // Seed rows using the exact legacy tables so this fixture exercises the v3-to-current path.
    sqlx::query(
        "INSERT INTO mailbox_messages
         (id, receiver_thread_id, submission_key, sender_key, payload_json, state)
         VALUES (?, ?, ?, ?, ?, 'claimed')",
    )
    .bind(claimed_message_id)
    .bind(receiver.to_string())
    .bind("claimed")
    .bind("a")
    .bind("{}")
    .execute(pool.as_ref())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mailbox_messages
         (id, receiver_thread_id, submission_key, sender_key, payload_json, state)
         VALUES (?, ?, ?, ?, ?, 'pending')",
    )
    .bind(pending_message_id)
    .bind(receiver.to_string())
    .bind("pending")
    .bind("user")
    .bind("{}")
    .execute(pool.as_ref())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mailbox_claims (receiver_thread_id, turn_id, tool_call_id, selection_json)
         VALUES (?, ?, ?, ?)",
    )
    .bind(receiver.to_string())
    .bind(&invocation.turn_id)
    .bind(&invocation.tool_call_id)
    .bind(serde_json::to_string(&MailboxSelection::All).unwrap())
    .execute(pool.as_ref())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mailbox_claim_members
         (receiver_thread_id, turn_id, tool_call_id, message_id, delivery_id)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(receiver.to_string())
    .bind(&invocation.turn_id)
    .bind(&invocation.tool_call_id)
    .bind(claimed_message_id)
    .bind(delivery_id)
    .execute(pool.as_ref())
    .await
    .unwrap();
    let revisions = queue
        .changes_since(/*revision*/ 0, &[receiver])
        .await
        .unwrap();
    let pending = MailboxMessage {
        id: pending_message_id.to_string(),
        receiver_thread_id: receiver,
        submission_key: "pending".to_string(),
        sender_key: "user".to_string(),
        payload_json: "{}".to_string(),
        acceptance_sequence: 2,
        state: MailboxMessageState::Pending,
        rejection_reason: None,
        final_subscription: None,
    };
    let claim = MailboxClaim {
        invocation,
        selection: MailboxSelection::All,
        messages: vec![MailboxClaimedMessage {
            message: MailboxMessage {
                id: claimed_message_id.to_string(),
                receiver_thread_id: receiver,
                submission_key: "claimed".to_string(),
                sender_key: "a".to_string(),
                payload_json: "{}".to_string(),
                acceptance_sequence: 1,
                state: MailboxMessageState::Claimed,
                rejection_reason: None,
                final_subscription: None,
            },
            delivery_id: delivery_id.to_string(),
        }],
    };
    QUEUE_MIGRATOR.run(pool.as_ref()).await.unwrap();
    let prepared = queue
        .prepare_mail_inventory(receiver)
        .await
        .unwrap()
        .unwrap();
    queue
        .acknowledge_mail_inventory(receiver, &prepared.id)
        .await
        .unwrap();
    assert_eq!(
        queue.read_mail(receiver, &pending.id).await.unwrap(),
        Some(pending)
    );
    assert_eq!(
        queue.list_unconsumed_mail_claims(receiver).await.unwrap(),
        vec![claim]
    );
    assert_eq!(
        queue
            .list_page(receiver, /*offset*/ 0, /*limit*/ 1)
            .await
            .unwrap(),
        vec![ordinary]
    );
    assert_eq!(
        queue
            .changes_since(/*revision*/ 0, &[receiver])
            .await
            .unwrap(),
        revisions
    );
}
