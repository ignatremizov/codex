use super::LocalThreadStore;
use crate::MailboxInventory;
use crate::MailboxInventoryAcknowledgement;
use crate::MailboxInventoryAcknowledgementOutcome;
use crate::MailboxInventoryNotification;
use crate::MailboxInventoryRecovery;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::mailbox::storage_error;
use crate::mailbox_inventory::decode_inventory;
use crate::mailbox_inventory::decode_notification;
use crate::mailbox_inventory_artifacts::recovery_error;
use codex_protocol::ThreadId;

pub(super) async fn read(
    store: &LocalThreadStore,
    receiver: ThreadId,
) -> ThreadStoreResult<MailboxInventory> {
    let state = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "read_mailbox_inventory",
        })?;
    decode_inventory(
        state
            .thread_queue()
            .read_mail_inventory(receiver)
            .await
            .map_err(|error| storage_error(error.as_ref()))?,
    )
}

pub(super) async fn has_pending(
    store: &LocalThreadStore,
    notification: MailboxInventoryNotification,
) -> ThreadStoreResult<bool> {
    notification.context()?;
    let state = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "has_pending_mailbox_inventory",
        })?;
    state
        .thread_queue()
        .has_pending_mail_at_or_before(
            notification.receiver_thread_id,
            notification.through_sequence,
        )
        .await
        .map_err(|error| storage_error(error.as_ref()))
}

pub(super) async fn prepare(
    store: &LocalThreadStore,
    receiver: ThreadId,
) -> ThreadStoreResult<Option<MailboxInventoryNotification>> {
    let state = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "prepare_mailbox_inventory",
        })?;
    state
        .thread_queue()
        .prepare_mail_inventory(receiver)
        .await
        .map_err(|error| storage_error(error.as_ref()))?
        .map(decode_notification)
        .transpose()
}

pub(super) async fn recover(
    store: &LocalThreadStore,
    notification: MailboxInventoryNotification,
) -> ThreadStoreResult<MailboxInventoryRecovery> {
    let inventory = read(store, notification.receiver_thread_id).await?;
    let history = super::mailbox::load_history(store, notification.receiver_thread_id).await?;
    crate::mailbox_inventory_artifacts::recover(&notification, &inventory, &history)
}

pub(super) async fn reconcile(
    store: &LocalThreadStore,
    notification: MailboxInventoryNotification,
) -> ThreadStoreResult<MailboxInventoryAcknowledgement> {
    let (notified_through, outcome) = match recover(store, notification.clone()).await? {
        MailboxInventoryRecovery::NotRecorded => return Err(recovery_error(&notification)),
        MailboxInventoryRecovery::Recorded { .. } => {
            let state = store
                .state_db
                .as_ref()
                .ok_or(ThreadStoreError::Unsupported {
                    operation: "reconcile_mailbox_inventory",
                })?;
            let watermark = state
                .thread_queue()
                .acknowledge_mail_inventory(notification.receiver_thread_id, &notification.id)
                .await
                .map_err(|error| storage_error(error.as_ref()))?;
            // Return the committed result directly: no fallible read after ack.
            (
                watermark,
                MailboxInventoryAcknowledgementOutcome::Acknowledged,
            )
        }
        MailboxInventoryRecovery::AlreadyCovered {
            notified_through, ..
        } => (
            notified_through,
            MailboxInventoryAcknowledgementOutcome::AlreadyCovered,
        ),
    };
    Ok(MailboxInventoryAcknowledgement {
        notification_id: notification.id,
        notified_through,
        outcome,
    })
}

pub(super) async fn cancel(
    store: &LocalThreadStore,
    notification: MailboxInventoryNotification,
) -> ThreadStoreResult<()> {
    if !matches!(
        recover(store, notification.clone()).await?,
        MailboxInventoryRecovery::NotRecorded
    ) {
        return Err(recovery_error(&notification));
    }
    let state = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "cancel_mailbox_inventory",
        })?;
    state
        .thread_queue()
        .cancel_mail_inventory(notification.receiver_thread_id, &notification.id)
        .await
        .map_err(|error| storage_error(error.as_ref()))
}
