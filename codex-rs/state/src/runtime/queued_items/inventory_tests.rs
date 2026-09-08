use super::*;
use crate::MailboxInvocation;
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
    queue
        .accept_mail(receiver, "claimed", "a", "{}")
        .await
        .unwrap();
    let invocation = MailboxInvocation {
        receiver_thread_id: receiver,
        turn_id: "existing-turn".to_string(),
        tool_call_id: "existing-call".to_string(),
    };
    let claim = queue
        .claim_mail(&invocation, &MailboxSelection::All)
        .await
        .unwrap();
    let pending = queue
        .accept_mail(receiver, "pending", "user", "{}")
        .await
        .unwrap();
    let revisions = queue
        .changes_since(/*revision*/ 0, &[receiver])
        .await
        .unwrap();
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
