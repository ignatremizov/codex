//! Proof for inventory context in raw receiver-owned history, not model replay.

use crate::MailboxInventory;
use crate::MailboxInventoryNotification;
use crate::MailboxInventoryRecovery;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_rollout::ResponseItemEnvelope;
use codex_rollout::RolloutItem;

pub(crate) fn recovery_error(notification: &MailboxInventoryNotification) -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: format!(
            "mailbox inventory recovery required for receiver {} notification {}: \
             missing, conflicting, or stale canonical proof; not permission to wake or cancel",
            notification.receiver_thread_id, notification.id
        ),
    }
}

/// Matches all immutable fields while preserving the actual envelope's stamps.
pub(crate) fn canonical_context(
    notification: &MailboxInventoryNotification,
    history: &[RolloutItem],
) -> ThreadStoreResult<Option<ResponseItemEnvelope>> {
    let expected = notification.context()?;
    let mut found: Option<&ResponseItemEnvelope> = None;
    for item in history {
        match item {
            RolloutItem::ResponseItem(envelope) if envelope.id() == expected.id() => {
                let matches = match (&envelope.item, &expected.item) {
                    (
                        ResponseItem::Message {
                            role,
                            content,
                            phase,
                            ..
                        },
                        ResponseItem::Message {
                            content: expected_content,
                            ..
                        },
                    ) => {
                        role == "developer"
                            && content == expected_content
                            && phase.is_none()
                            && envelope.item.turn_id() == Some(notification.id.as_str())
                    }
                    _ => false,
                };
                if !matches || found.is_some_and(|previous| previous != envelope) {
                    return Err(recovery_error(notification));
                }
                found = Some(envelope);
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if expected
                    .id()
                    .is_some_and(|id| id.as_str() == event.item.id()) =>
            {
                // Inventory is harness context, never typed user/agent task input.
                return Err(recovery_error(notification));
            }
            _ => {}
        }
    }
    Ok(found.cloned())
}

/// A retired notification needs both its original proof and covered watermark.
/// The caller retains its original immutable snapshot across uncertain receipts.
pub(crate) fn recover(
    notification: &MailboxInventoryNotification,
    inventory: &MailboxInventory,
    history: &[RolloutItem],
) -> ThreadStoreResult<MailboxInventoryRecovery> {
    if inventory.receiver_thread_id != notification.receiver_thread_id {
        return Err(recovery_error(notification));
    }
    let active_matches = inventory.active_notification.as_ref() == Some(notification);
    if inventory
        .active_notification
        .as_ref()
        .is_some_and(|active| active.id == notification.id && active != notification)
    {
        return Err(recovery_error(notification));
    }
    let context = canonical_context(notification, history)?;
    match context {
        Some(context) if inventory.notified_through >= notification.through_sequence => {
            Ok(MailboxInventoryRecovery::AlreadyCovered {
                context,
                notified_through: inventory.notified_through,
            })
        }
        Some(context) if active_matches => Ok(MailboxInventoryRecovery::Recorded { context }),
        None if active_matches => Ok(MailboxInventoryRecovery::NotRecorded),
        Some(_) | None => Err(recovery_error(notification)),
    }
}
