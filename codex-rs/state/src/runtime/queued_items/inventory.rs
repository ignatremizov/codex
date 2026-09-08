//! Pending-mail inventory and notification progress, separate from message consumption.
//!
//! ThreadStore must verify canonical inventory context before acknowledgement and prove
//! absence under its durable delivery permit before cancellation. SQL does neither.

use super::SqliteQueueStore;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::SqliteConnection;
use uuid::Uuid;

/// Counts for one canonical sender; no accepted payload is loaded into an inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxSenderInventory {
    pub sender_key: String,
    pub count: u64,
    pub max_acceptance_sequence: i64,
}

/// Immutable pending-mail snapshot with a stable canonical presentation identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInventoryNotification {
    pub id: String,
    pub receiver_thread_id: ThreadId,
    pub through_sequence: i64,
    pub pending_senders: Vec<MailboxSenderInventory>,
}

/// Current inventory and notification progress, read from one SQL snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInventory {
    pub receiver_thread_id: ThreadId,
    pub notified_through: i64,
    /// Selectable mail, grouped in canonical sender-key order.
    pub pending_senders: Vec<MailboxSenderInventory>,
    /// Unresolved fixed-invocation members, not selectable by a new claim.
    /// Their recovery remains independent of the notification watermark.
    pub claimed_senders: Vec<MailboxSenderInventory>,
    pub active_notification: Option<MailboxInventoryNotification>,
}

fn stale_notification() -> anyhow::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "mailbox inventory notification is not active for this receiver",
    )
    .into()
}

async fn read_inventory(
    connection: &mut SqliteConnection,
    receiver_thread_id: ThreadId,
) -> anyhow::Result<MailboxInventory> {
    let receiver = receiver_thread_id.to_string();
    let progress = sqlx::query(
        "SELECT notified_through, notification_id, notification_through, pending_senders_json
         FROM mailbox_inventory_state WHERE receiver_thread_id = ?",
    )
    .bind(&receiver)
    .fetch_optional(&mut *connection)
    .await?;
    let mut inventory = MailboxInventory {
        receiver_thread_id,
        notified_through: 0,
        pending_senders: Vec::new(),
        claimed_senders: Vec::new(),
        active_notification: None,
    };
    if let Some(progress) = progress {
        inventory.notified_through = progress.try_get("notified_through")?;
        if let Some(id) = progress.try_get::<Option<String>, _>("notification_id")? {
            inventory.active_notification = Some(MailboxInventoryNotification {
                id,
                receiver_thread_id,
                through_sequence: progress.try_get("notification_through")?,
                pending_senders: serde_json::from_str(progress.try_get("pending_senders_json")?)?,
            });
        }
    }
    let groups = sqlx::query(
        "SELECT state, sender_key, COUNT(*) AS message_count,
                MAX(acceptance_sequence) AS max_acceptance_sequence
         FROM mailbox_messages
         WHERE receiver_thread_id = ? AND state IN ('pending', 'claimed')
         GROUP BY state, sender_key ORDER BY sender_key",
    )
    .bind(receiver)
    .fetch_all(connection)
    .await?;
    for group in groups {
        let sender = MailboxSenderInventory {
            sender_key: group.try_get("sender_key")?,
            count: u64::try_from(group.try_get::<i64, _>("message_count")?)?,
            max_acceptance_sequence: group.try_get("max_acceptance_sequence")?,
        };
        match group.try_get::<&str, _>("state")? {
            "pending" => inventory.pending_senders.push(sender),
            "claimed" => inventory.claimed_senders.push(sender),
            _ => anyhow::bail!("invalid mailbox inventory state"),
        }
    }
    Ok(inventory)
}

impl SqliteQueueStore {
    /// Tests the exact pending frontier without reading payloads or changing progress.
    pub async fn has_pending_mail_at_or_before(
        &self,
        receiver_thread_id: ThreadId,
        through_sequence: i64,
    ) -> anyhow::Result<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM mailbox_messages
                WHERE receiver_thread_id = ? AND state = 'pending'
                  AND acceptance_sequence <= ?
             )",
        )
        .bind(receiver_thread_id.to_string())
        .bind(through_sequence)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(Into::into)
    }

    /// Reads counts, the notified watermark, and any fixed preparation without mutating mail.
    pub async fn read_mail_inventory(
        &self,
        receiver_thread_id: ThreadId,
    ) -> anyhow::Result<MailboxInventory> {
        let mut tx = self.pool.begin().await?;
        let inventory = read_inventory(&mut tx, receiver_thread_id).await?;
        tx.commit().await?;
        Ok(inventory)
    }

    /// Recovers the active preparation, or fixes a new snapshot if newer pending mail exists.
    ///
    /// Counts include all pending mail through the fixed frontier, including older unread mail.
    /// New arrivals cannot join an existing preparation. An obsolete preparation is still returned:
    /// the caller must reconcile canonical evidence before acknowledging or cancelling it.
    pub async fn prepare_mail_inventory(
        &self,
        receiver_thread_id: ThreadId,
    ) -> anyhow::Result<Option<MailboxInventoryNotification>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let inventory = read_inventory(&mut tx, receiver_thread_id).await?;
        if let Some(notification) = inventory.active_notification {
            tx.commit().await?;
            return Ok(Some(notification));
        }
        let through_sequence = inventory
            .pending_senders
            .iter()
            .map(|sender| sender.max_acceptance_sequence)
            .max();
        let Some(through_sequence) =
            through_sequence.filter(|sequence| *sequence > inventory.notified_through)
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let notification = MailboxInventoryNotification {
            id: Uuid::now_v7().to_string(),
            receiver_thread_id,
            through_sequence,
            pending_senders: inventory.pending_senders,
        };
        sqlx::query(
            "INSERT INTO mailbox_inventory_state
             (receiver_thread_id, notification_id, notification_through, pending_senders_json)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (receiver_thread_id) DO UPDATE SET
                notification_id = excluded.notification_id,
                notification_through = excluded.notification_through,
                pending_senders_json = excluded.pending_senders_json",
        )
        .bind(receiver_thread_id.to_string())
        .bind(&notification.id)
        .bind(notification.through_sequence)
        .bind(serde_json::to_string(&notification.pending_senders)?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(notification))
    }

    /// Advances notification progress and retires only the matching active preparation.
    ///
    /// ThreadStore must first verify its fixed inventory context in canonical receiver history.
    /// This does not consume mail. Stale IDs, including retries after retirement, return an error
    /// and can never acknowledge a newer preparation.
    pub async fn acknowledge_mail_inventory(
        &self,
        receiver_thread_id: ThreadId,
        notification_id: &str,
    ) -> anyhow::Result<i64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let watermark = sqlx::query_scalar(
            "UPDATE mailbox_inventory_state SET
                notified_through = MAX(notified_through, notification_through),
                notification_id = NULL, notification_through = NULL, pending_senders_json = NULL
             WHERE receiver_thread_id = ? AND notification_id = ?
             RETURNING notified_through",
        )
        .bind(receiver_thread_id.to_string())
        .bind(notification_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(stale_notification)?;
        tx.commit().await?;
        Ok(watermark)
    }

    /// Retires a matching preparation without advancing notification or consumption state.
    ///
    /// ThreadStore must prove canonical absence while holding its durable delivery permit.
    /// This primitive does not perform that check or decide whether cancellation is appropriate.
    pub async fn cancel_mail_inventory(
        &self,
        receiver_thread_id: ThreadId,
        notification_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE mailbox_inventory_state SET notification_id = NULL,
                notification_through = NULL, pending_senders_json = NULL
             WHERE receiver_thread_id = ? AND notification_id = ?",
        )
        .bind(receiver_thread_id.to_string())
        .bind(notification_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(stale_notification());
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod tests;
