//! Conditional-final bindings and retirement never change accepted mailbox payload or claims.
//! Graph authority is validated by callers across a separate database boundary.

#[cfg(test)]
#[path = "../mailbox_final_subscriptions_tests.rs"]
mod tests;

use super::MAILBOX_MESSAGE_WITH_SUBSCRIPTION;
use super::MailboxFinalSubscription;
use super::SqliteQueueStore;
use super::invalid_request;
use super::message_from_row;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;

impl SqliteQueueStore {
    /// Reads the active pending or bound subscription for one sender/receiver pair.
    pub async fn read_active_mailbox_final_subscription(
        &self,
        receiver_thread_id: ThreadId,
        sender_thread_id: ThreadId,
    ) -> anyhow::Result<Option<MailboxFinalSubscription>> {
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
        query
            .push(" WHERE s.receiver_thread_id = ")
            .push_bind(receiver_thread_id.to_string())
            .push(" AND s.sender_thread_id = ")
            .push_bind(sender_thread_id.to_string())
            .push(" AND s.state IN ('pending', 'bound')");
        let row = query.build().fetch_optional(self.pool.as_ref()).await?;
        row.as_ref()
            .map(message_from_row)
            .transpose()
            .map(|message| message.and_then(|message| message.final_subscription))
    }

    /// Reads one subscription by accepted message identity, including terminal rows.
    pub async fn read_mailbox_final_subscription(
        &self,
        receiver_thread_id: ThreadId,
        message_id: &str,
    ) -> anyhow::Result<Option<MailboxFinalSubscription>> {
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
        query
            .push(" WHERE s.receiver_thread_id = ")
            .push_bind(receiver_thread_id.to_string())
            .push(" AND s.message_id = ")
            .push_bind(message_id);
        let row = query.build().fetch_optional(self.pool.as_ref()).await?;
        row.as_ref()
            .map(message_from_row)
            .transpose()
            .map(|message| message.and_then(|message| message.final_subscription))
    }

    /// Lists active subscriptions involving either endpoint of a restored thread.
    pub async fn read_active_mailbox_final_subscriptions_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Vec<MailboxFinalSubscription>> {
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
        query
            .push(" WHERE s.state IN ('pending', 'bound')")
            .push(" AND (s.receiver_thread_id = ")
            .push_bind(thread_id.to_string())
            .push(" OR s.sender_thread_id = ")
            .push_bind(thread_id.to_string())
            .push(") ORDER BY m.acceptance_sequence");
        let rows = query.build().fetch_all(self.pool.as_ref()).await?;
        rows.iter()
            .map(message_from_row)
            .map(|message| {
                message.and_then(|message| {
                    message.final_subscription.ok_or_else(|| {
                        anyhow::anyhow!("active mailbox final subscription is missing")
                    })
                })
            })
            .collect()
    }

    /// Supersedes the sender's active final subscription after an ordinary
    /// final-observation update for this exact receiver.
    pub async fn supersede_mailbox_final_subscription(
        &self,
        receiver_thread_id: ThreadId,
        sender_thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE mailbox_final_subscriptions SET state = 'superseded'
             WHERE receiver_thread_id = ? AND sender_thread_id = ?
               AND state IN ('pending', 'bound')",
        )
        .bind(receiver_thread_id.to_string())
        .bind(sender_thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Retires one exact pending/bound subscription without affecting a later acceptance.
    pub async fn supersede_mailbox_final_subscription_message(
        &self,
        receiver_thread_id: ThreadId,
        message_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE mailbox_final_subscriptions SET state = 'superseded'
             WHERE receiver_thread_id = ? AND message_id = ?
               AND state IN ('pending', 'bound')",
        )
        .bind(receiver_thread_id.to_string())
        .bind(message_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Retires active subscriptions touching any endpoint in a lifecycle-revoked subtree.
    pub async fn supersede_mailbox_final_subscriptions_for_threads(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<()> {
        if thread_ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for thread_id in thread_ids {
            sqlx::query(
                "UPDATE mailbox_final_subscriptions SET state = 'superseded'
                 WHERE state IN ('pending', 'bound')
                   AND (receiver_thread_id = ? OR sender_thread_id = ?)",
            )
            .bind(thread_id.to_string())
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Marks a subscription delivered after its existing response-observation
    /// commit receipt is durably recorded by the observer.
    pub async fn acknowledge_mailbox_final_subscription_delivery(
        &self,
        receiver_thread_id: ThreadId,
        message_id: &str,
        turn_id: &str,
    ) -> anyhow::Result<()> {
        if turn_id.is_empty() {
            return Err(invalid_request(
                "mailbox final subscription turn must be nonempty",
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE mailbox_final_subscriptions SET state = 'delivered'
             WHERE receiver_thread_id = ? AND message_id = ? AND state = 'bound'
               AND bound_turn_id = ?",
        )
        .bind(receiver_thread_id.to_string())
        .bind(message_id)
        .bind(turn_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            let current = sqlx::query(
                "SELECT state, bound_turn_id FROM mailbox_final_subscriptions
                 WHERE receiver_thread_id = ? AND message_id = ?",
            )
            .bind(receiver_thread_id.to_string())
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                invalid_request("mailbox final subscription delivery does not match its bound turn")
            })?;
            let state: String = current.try_get("state")?;
            let bound_turn_id: Option<String> = current.try_get("bound_turn_id")?;
            if !matches!(state.as_str(), "delivered" | "superseded")
                || bound_turn_id.as_deref() != Some(turn_id)
            {
                return Err(invalid_request(
                    "mailbox final subscription delivery does not match its bound turn",
                ));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Returns subscriptions currently bound to an exact receiver turn.
    pub async fn read_bound_mailbox_final_subscriptions(
        &self,
        receiver_thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Vec<MailboxFinalSubscription>> {
        let mut connection = self.pool.acquire().await?;
        read_bound_final_subscriptions(&mut connection, receiver_thread_id, turn_id).await
    }
}

pub(in super::super) async fn bind_mailbox_final_subscriptions_for_inventory(
    connection: &mut SqliteConnection,
    receiver_thread_id: ThreadId,
    notification_id: &str,
    through_sequence: i64,
) -> anyhow::Result<Vec<MailboxFinalSubscription>> {
    sqlx::query(
        "UPDATE mailbox_final_subscriptions
         SET state = 'bound', bound_turn_id = ?
         WHERE receiver_thread_id = ? AND state = 'pending'
           AND message_id IN (
             SELECT id FROM mailbox_messages
             WHERE receiver_thread_id = ? AND state = 'pending'
               AND acceptance_sequence <= ?
           )",
    )
    .bind(notification_id)
    .bind(receiver_thread_id.to_string())
    .bind(receiver_thread_id.to_string())
    .bind(through_sequence)
    .execute(&mut *connection)
    .await?;
    read_bound_final_subscriptions(connection, receiver_thread_id, notification_id).await
}

async fn read_bound_final_subscriptions(
    connection: &mut SqliteConnection,
    receiver_thread_id: ThreadId,
    turn_id: &str,
) -> anyhow::Result<Vec<MailboxFinalSubscription>> {
    let rows = sqlx::query(
        "SELECT m.*,
                s.sender_thread_id AS final_subscription_sender_thread_id,
                s.state AS final_subscription_state,
                s.bound_turn_id AS final_subscription_bound_turn_id,
                s.receiver_lifecycle_epoch AS final_subscription_receiver_lifecycle_epoch,
                s.sender_lifecycle_epoch AS final_subscription_sender_lifecycle_epoch
         FROM mailbox_final_subscriptions s
         JOIN mailbox_messages m
           ON m.receiver_thread_id = s.receiver_thread_id AND m.id = s.message_id
         WHERE s.receiver_thread_id = ? AND s.bound_turn_id = ?
           AND s.state = 'bound'
         ORDER BY m.acceptance_sequence",
    )
    .bind(receiver_thread_id.to_string())
    .bind(turn_id)
    .fetch_all(&mut *connection)
    .await?;
    rows.iter()
        .map(message_from_row)
        .map(|message| message.map(|message| message.final_subscription))
        .map(|subscription| {
            subscription?.ok_or_else(|| anyhow::anyhow!("mailbox final subscription is missing"))
        })
        .collect()
}
