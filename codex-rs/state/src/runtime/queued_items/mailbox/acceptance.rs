//! Immutable mailbox acceptance and conditional-final intent share one queue transaction.

use super::MAILBOX_MESSAGE_WITH_SUBSCRIPTION;
use super::MailboxFinalSubscriptionAuthority;
use super::MailboxFinalSubscriptionRequest;
use super::MailboxMessage;
use super::SqliteQueueStore;
use super::invalid_request;
use super::message_from_row;
use codex_protocol::ThreadId;
use sqlx::Sqlite;
use uuid::Uuid;

impl SqliteQueueStore {
    /// Accepts immutable content, idempotently scoped by receiver and submission key.
    ///
    /// Sender keys are canonical identities supplied by the caller, not grants. Payloads are
    /// opaque JSON strings compared byte-for-byte; no size limit or normalization is applied.
    pub async fn accept_mail(
        &self,
        receiver_thread_id: ThreadId,
        submission_key: &str,
        sender_key: &str,
        payload_json: &str,
    ) -> anyhow::Result<MailboxMessage> {
        self.accept_mail_with_final_subscription(
            receiver_thread_id,
            submission_key,
            sender_key,
            payload_json,
            MailboxFinalSubscriptionRequest::None,
        )
        .await
    }

    /// Accepts immutable content and, for `Wake`, atomically replaces the
    /// sender's active mailbox-final subscription for this receiver.
    pub async fn accept_mail_with_final_subscription(
        &self,
        receiver_thread_id: ThreadId,
        submission_key: &str,
        sender_key: &str,
        payload_json: &str,
        final_subscription_request: MailboxFinalSubscriptionRequest,
    ) -> anyhow::Result<MailboxMessage> {
        let authority = (final_subscription_request == MailboxFinalSubscriptionRequest::Wake)
            .then_some(MailboxFinalSubscriptionAuthority {
                receiver_lifecycle_epoch: 0,
                sender_lifecycle_epoch: 0,
            });
        self.accept_mail_with_final_subscription_and_authority(
            receiver_thread_id,
            submission_key,
            sender_key,
            payload_json,
            final_subscription_request,
            authority,
        )
        .await
    }

    /// Accepts mailbox input with graph authority captured by the Core admission boundary.
    pub async fn accept_mail_with_final_subscription_and_authority(
        &self,
        receiver_thread_id: ThreadId,
        submission_key: &str,
        sender_key: &str,
        payload_json: &str,
        final_subscription_request: MailboxFinalSubscriptionRequest,
        final_subscription_authority: Option<MailboxFinalSubscriptionAuthority>,
    ) -> anyhow::Result<MailboxMessage> {
        if submission_key.is_empty() || sender_key.is_empty() {
            return Err(invalid_request(
                "mailbox submission and sender keys must be nonempty",
            ));
        }
        let sender_thread_id = match final_subscription_request {
            MailboxFinalSubscriptionRequest::None => None,
            MailboxFinalSubscriptionRequest::Wake => Some(
                sender_key
                    .strip_prefix("agent:")
                    .and_then(|sender| ThreadId::from_string(sender).ok())
                    .ok_or_else(|| {
                        invalid_request(
                            "mailbox final subscriptions require a canonical agent sender",
                        )
                    })?,
            ),
        };
        match (final_subscription_request, final_subscription_authority) {
            (MailboxFinalSubscriptionRequest::None, None) => {}
            (MailboxFinalSubscriptionRequest::Wake, Some(authority))
                if authority.receiver_lifecycle_epoch >= 0
                    && authority.sender_lifecycle_epoch >= 0 => {}
            _ => {
                return Err(invalid_request(
                    "mailbox final subscriptions require nonnegative endpoint authority epochs",
                ));
            }
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let inserted = sqlx::query(
            "INSERT INTO mailbox_messages
             (id, receiver_thread_id, submission_key, sender_key, payload_json)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (receiver_thread_id, submission_key) DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(receiver_thread_id.to_string())
        .bind(submission_key)
        .bind(sender_key)
        .bind(payload_json)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if let Some(sender_thread_id) = sender_thread_id
            && inserted != 0
        {
            let authority = final_subscription_authority.ok_or_else(|| {
                invalid_request("mailbox final subscription authority is missing")
            })?;
            sqlx::query(
                "UPDATE mailbox_final_subscriptions SET state = 'superseded'
                 WHERE receiver_thread_id = ? AND sender_thread_id = ?
                   AND state IN ('pending', 'bound')",
            )
            .bind(receiver_thread_id.to_string())
            .bind(sender_thread_id.to_string())
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO mailbox_final_subscriptions
                 (receiver_thread_id, message_id, sender_thread_id, state,
                  receiver_lifecycle_epoch, sender_lifecycle_epoch)
                 SELECT receiver_thread_id, id, ?, 'pending', ?, ?
                 FROM mailbox_messages
                 WHERE receiver_thread_id = ? AND submission_key = ?",
            )
            .bind(sender_thread_id.to_string())
            .bind(authority.receiver_lifecycle_epoch)
            .bind(authority.sender_lifecycle_epoch)
            .bind(receiver_thread_id.to_string())
            .bind(submission_key)
            .execute(&mut *tx)
            .await?;
        }
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
        query
            .push(" WHERE m.receiver_thread_id = ")
            .push_bind(receiver_thread_id.to_string())
            .push(" AND m.submission_key = ")
            .push_bind(submission_key);
        let row = query.build().fetch_one(&mut *tx).await?;
        let message = message_from_row(&row)?;
        let requested_wake = final_subscription_request == MailboxFinalSubscriptionRequest::Wake;
        if message.sender_key != sender_key
            || message.payload_json != payload_json
            || message.final_subscription.is_some() != requested_wake
        {
            return Err(invalid_request(
                "mailbox submission key already has different content or final subscription intent",
            ));
        }
        tx.commit().await?;
        Ok(message)
    }
}
