//! Durable acceptance and fixed-batch claims, independent of turn admission and authority.
//!
//! Callers must verify canonical receiver history before acknowledging delivery. These SQL
//! transactions do not make history appends and mailbox acknowledgement atomic.

use super::SqliteQueueStore;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

const MAILBOX_MESSAGE_WITH_SUBSCRIPTION: &str = "SELECT m.*,
    s.sender_thread_id AS final_subscription_sender_thread_id,
    s.state AS final_subscription_state,
    s.bound_turn_id AS final_subscription_bound_turn_id,
    s.receiver_lifecycle_epoch AS final_subscription_receiver_lifecycle_epoch,
    s.sender_lifecycle_epoch AS final_subscription_sender_lifecycle_epoch
    FROM mailbox_messages m
    LEFT JOIN mailbox_final_subscriptions s
      ON s.receiver_thread_id = m.receiver_thread_id AND s.message_id = m.id";

/// A tool invocation scoped to the receiver and its originating turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInvocation {
    pub receiver_thread_id: ThreadId,
    pub turn_id: String,
    pub tool_call_id: String,
}

/// Canonical sender selection; sender sets are sorted and deduplicated when claimed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxSelection {
    All,
    Senders(Vec<String>),
}

/// Durable delivery state. Acceptance alone never means model visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxMessageState {
    Pending,
    Claimed,
    Consumed,
    Rejected,
}

/// Immutable accepted content with its current delivery state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxMessage {
    pub id: String,
    pub receiver_thread_id: ThreadId,
    pub submission_key: String,
    pub sender_key: String,
    pub payload_json: String,
    pub acceptance_sequence: i64,
    pub state: MailboxMessageState,
    pub rejection_reason: Option<String>,
    pub final_subscription: Option<MailboxFinalSubscription>,
}

/// Whether acceptance requests a final response observation after a qualifying
/// mailbox opportunity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MailboxFinalSubscriptionRequest {
    #[default]
    None,
    Wake,
}

/// Current durable state of the final-response opportunity for one accepted message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxFinalSubscriptionState {
    Pending,
    Bound,
    Delivered,
    Superseded,
    Rejected,
}

/// Graph authority captured for both endpoints when a final subscription is accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailboxFinalSubscriptionAuthority {
    pub receiver_lifecycle_epoch: i64,
    pub sender_lifecycle_epoch: i64,
}

/// A subscription is identified by the accepted message, and its bound turn is
/// retained after delivery or supersession for exact recovery/audit evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxFinalSubscription {
    pub message_id: String,
    pub receiver_thread_id: ThreadId,
    pub sender_thread_id: ThreadId,
    pub acceptance_sequence: i64,
    pub state: MailboxFinalSubscriptionState,
    pub bound_turn_id: Option<String>,
    pub receiver_lifecycle_epoch: i64,
    pub sender_lifecycle_epoch: i64,
}

/// The state committed with the inventory acknowledgement, including exact
/// subscriptions whose accepted mail was covered by that inventory frontier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInventoryAcknowledgement {
    pub notified_through: i64,
    pub bound_subscriptions: Vec<MailboxFinalSubscription>,
}

/// One fixed claim member and its reserved receiver-history delivery identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxClaimedMessage {
    pub message: MailboxMessage,
    pub delivery_id: String,
}

/// Fixed membership in acceptance order, with current per-message delivery state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxClaim {
    pub invocation: MailboxInvocation,
    pub selection: MailboxSelection,
    pub messages: Vec<MailboxClaimedMessage>,
}

fn invalid_request(message: &str) -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into()
}

fn message_from_row(row: &SqliteRow) -> anyhow::Result<MailboxMessage> {
    let state: String = row.try_get("state")?;
    let state = match state.as_str() {
        "pending" => MailboxMessageState::Pending,
        "claimed" => MailboxMessageState::Claimed,
        "consumed" => MailboxMessageState::Consumed,
        "rejected" => MailboxMessageState::Rejected,
        _ => anyhow::bail!("invalid stored mailbox state"),
    };
    let final_subscription = row
        .try_get::<Option<String>, _>("final_subscription_state")?
        .map(|state| {
            let state = match state.as_str() {
                "pending" => MailboxFinalSubscriptionState::Pending,
                "bound" => MailboxFinalSubscriptionState::Bound,
                "delivered" => MailboxFinalSubscriptionState::Delivered,
                "superseded" => MailboxFinalSubscriptionState::Superseded,
                "rejected" => MailboxFinalSubscriptionState::Rejected,
                _ => anyhow::bail!("invalid stored mailbox final subscription state"),
            };
            let bound_turn_id: Option<String> = row.try_get("final_subscription_bound_turn_id")?;
            let sender_thread_id = ThreadId::try_from(
                row.try_get::<String, _>("final_subscription_sender_thread_id")?,
            )?;
            let message_id = row.try_get("id")?;
            let receiver_thread_id =
                ThreadId::try_from(row.try_get::<String, _>("receiver_thread_id")?)?;
            let acceptance_sequence = row.try_get("acceptance_sequence")?;
            let receiver_lifecycle_epoch =
                row.try_get("final_subscription_receiver_lifecycle_epoch")?;
            let sender_lifecycle_epoch =
                row.try_get("final_subscription_sender_lifecycle_epoch")?;
            if receiver_lifecycle_epoch < 0 || sender_lifecycle_epoch < 0 {
                anyhow::bail!("invalid stored mailbox final subscription authority epoch");
            }
            match (state, bound_turn_id.as_ref()) {
                (MailboxFinalSubscriptionState::Pending, None)
                | (MailboxFinalSubscriptionState::Bound, Some(_))
                | (MailboxFinalSubscriptionState::Delivered, Some(_))
                | (MailboxFinalSubscriptionState::Superseded, _)
                | (MailboxFinalSubscriptionState::Rejected, _) => {}
                _ => anyhow::bail!("invalid stored mailbox final subscription turn binding"),
            }
            Ok(MailboxFinalSubscription {
                message_id,
                receiver_thread_id,
                sender_thread_id,
                acceptance_sequence,
                state,
                bound_turn_id,
                receiver_lifecycle_epoch,
                sender_lifecycle_epoch,
            })
        })
        .transpose()?;
    Ok(MailboxMessage {
        id: row.try_get("id")?,
        receiver_thread_id: ThreadId::try_from(row.try_get::<String, _>("receiver_thread_id")?)?,
        submission_key: row.try_get("submission_key")?,
        sender_key: row.try_get("sender_key")?,
        payload_json: row.try_get("payload_json")?,
        acceptance_sequence: row.try_get("acceptance_sequence")?,
        state,
        rejection_reason: row.try_get("rejection_reason")?,
        final_subscription,
    })
}

async fn read_message(
    connection: &mut SqliteConnection,
    receiver_thread_id: ThreadId,
    message_id: &str,
) -> anyhow::Result<MailboxMessage> {
    let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
    query
        .push(" WHERE m.receiver_thread_id = ")
        .push_bind(receiver_thread_id.to_string())
        .push(" AND m.id = ")
        .push_bind(message_id);
    let row = query
        .build()
        .fetch_optional(connection)
        .await?
        .ok_or_else(|| invalid_request("mailbox message does not exist for this receiver"))?;
    message_from_row(&row)
}

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

    /// Claims pending matching messages in one write-serialized transaction.
    ///
    /// Retrying an invocation returns its original members and delivery IDs, even when empty.
    /// Different invocations cannot share members. Terminal members remain in the returned batch
    /// for recovery; callers must inspect their state rather than inject them again.
    pub async fn claim_mail(
        &self,
        invocation: &MailboxInvocation,
        selection: &MailboxSelection,
    ) -> anyhow::Result<MailboxClaim> {
        if invocation.turn_id.is_empty() || invocation.tool_call_id.is_empty() {
            return Err(invalid_request(
                "mailbox invocation turn and tool call IDs must be nonempty",
            ));
        }
        let mut selection = selection.clone();
        match &mut selection {
            MailboxSelection::All => {}
            MailboxSelection::Senders(senders) => {
                if senders.iter().any(String::is_empty) {
                    return Err(invalid_request("mailbox sender keys must be nonempty"));
                }
                senders.sort();
                senders.dedup();
            }
        }
        let selection_json = serde_json::to_string(&selection)?;
        let receiver = invocation.receiver_thread_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let inserted = sqlx::query(
            "INSERT INTO mailbox_claims (receiver_thread_id, turn_id, tool_call_id, selection_json)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (receiver_thread_id, turn_id, tool_call_id) DO NOTHING",
        )
        .bind(&receiver)
        .bind(&invocation.turn_id)
        .bind(&invocation.tool_call_id)
        .bind(&selection_json)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let stored_selection: String = sqlx::query_scalar(
            "SELECT selection_json FROM mailbox_claims
             WHERE receiver_thread_id = ? AND turn_id = ? AND tool_call_id = ?",
        )
        .bind(&receiver)
        .bind(&invocation.turn_id)
        .bind(&invocation.tool_call_id)
        .fetch_one(&mut *tx)
        .await?;
        if stored_selection != selection_json {
            return Err(invalid_request(
                "mailbox invocation already has a different sender selection",
            ));
        }
        if inserted != 0 {
            let mut query = sqlx::QueryBuilder::<Sqlite>::new(
                "SELECT id FROM mailbox_messages WHERE state = 'pending' AND receiver_thread_id = ",
            );
            query.push_bind(&receiver);
            match &selection {
                MailboxSelection::All => {}
                MailboxSelection::Senders(senders) if senders.is_empty() => {
                    query.push(" AND 0");
                }
                MailboxSelection::Senders(senders) => {
                    query.push(" AND sender_key IN (");
                    let mut separated = query.separated(", ");
                    for sender in senders {
                        separated.push_bind(sender);
                    }
                    separated.push_unseparated(")");
                }
            }
            query.push(" ORDER BY acceptance_sequence");
            let message_ids: Vec<String> = query.build_query_scalar().fetch_all(&mut *tx).await?;
            for message_id in message_ids {
                sqlx::query(
                    "INSERT INTO mailbox_claim_members
                     (receiver_thread_id, turn_id, tool_call_id, message_id, delivery_id)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&receiver)
                .bind(&invocation.turn_id)
                .bind(&invocation.tool_call_id)
                .bind(&message_id)
                .bind(Uuid::now_v7().to_string())
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE mailbox_messages SET state = 'claimed' WHERE id = ?")
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let claim = read_claim(&mut tx, invocation)
            .await?
            .ok_or_else(|| invalid_request("mailbox claim is missing"))?;
        tx.commit().await?;
        Ok(claim)
    }

    /// Reads original selection and membership with current states, without establishing a claim.
    /// An existing empty or entirely terminal claim is distinct from a missing invocation.
    pub async fn lookup_mail_claim(
        &self,
        invocation: &MailboxInvocation,
    ) -> anyhow::Result<Option<MailboxClaim>> {
        if invocation.turn_id.is_empty() || invocation.tool_call_id.is_empty() {
            return Err(invalid_request(
                "mailbox invocation turn and tool call IDs must be nonempty",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let claim = read_claim(&mut tx, invocation).await?;
        tx.commit().await?;
        Ok(claim)
    }

    /// Lists existing claims with at least one claimed member for receiver-history recovery.
    ///
    /// Reads one SQL snapshot without creating claims or changing delivery state. Each claim
    /// includes its full original membership and current states, including terminal members.
    /// Claims are ordered by their earliest accepted member; members remain in acceptance order.
    /// An outstanding claim alone is not evidence that delivery reached canonical history.
    pub async fn list_unconsumed_mail_claims(
        &self,
        receiver_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<MailboxClaim>> {
        let rows = sqlx::query(
            "WITH outstanding AS (
                SELECT c.receiver_thread_id, c.turn_id, c.tool_call_id,
                       MIN(m.acceptance_sequence) AS first_sequence
                FROM mailbox_claim_members c
                JOIN mailbox_messages m ON m.id = c.message_id
                WHERE c.receiver_thread_id = ?
                GROUP BY c.receiver_thread_id, c.turn_id, c.tool_call_id
                HAVING MAX(m.state = 'claimed') = 1
             )
             SELECT m.*, c.delivery_id, c.turn_id, c.tool_call_id, claims.selection_json,
                    s.sender_thread_id AS final_subscription_sender_thread_id,
                    s.state AS final_subscription_state,
                    s.bound_turn_id AS final_subscription_bound_turn_id,
                    s.receiver_lifecycle_epoch AS final_subscription_receiver_lifecycle_epoch,
                    s.sender_lifecycle_epoch AS final_subscription_sender_lifecycle_epoch
             FROM outstanding o
             JOIN mailbox_claims claims
               USING (receiver_thread_id, turn_id, tool_call_id)
             JOIN mailbox_claim_members c
               USING (receiver_thread_id, turn_id, tool_call_id)
             JOIN mailbox_messages m ON m.id = c.message_id
             LEFT JOIN mailbox_final_subscriptions s
               ON s.receiver_thread_id = m.receiver_thread_id AND s.message_id = m.id
             ORDER BY o.first_sequence, m.acceptance_sequence",
        )
        .bind(receiver_thread_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;
        let mut claims: Vec<MailboxClaim> = Vec::new();
        for row in rows {
            let invocation = MailboxInvocation {
                receiver_thread_id,
                turn_id: row.try_get("turn_id")?,
                tool_call_id: row.try_get("tool_call_id")?,
            };
            if claims
                .last()
                .is_none_or(|claim| claim.invocation != invocation)
            {
                claims.push(MailboxClaim {
                    invocation,
                    selection: serde_json::from_str(row.try_get("selection_json")?)?,
                    messages: Vec::new(),
                });
            }
            if let Some(claim) = claims.last_mut() {
                claim.messages.push(MailboxClaimedMessage {
                    message: message_from_row(&row)?,
                    delivery_id: row.try_get("delivery_id")?,
                });
            }
        }
        Ok(claims)
    }

    /// Marks a claimed member consumed after the caller verifies canonical history delivery.
    ///
    /// This does not verify history itself and must not be called merely on selection or enqueue.
    /// Repeated acknowledgement of the same member is idempotent; rejection is terminal.
    pub async fn acknowledge_mail(
        &self,
        invocation: &MailboxInvocation,
        message_id: &str,
        delivery_id: &str,
    ) -> anyhow::Result<MailboxMessage> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let matched: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mailbox_claim_members
             WHERE receiver_thread_id = ? AND turn_id = ? AND tool_call_id = ?
               AND message_id = ? AND delivery_id = ?)",
        )
        .bind(invocation.receiver_thread_id.to_string())
        .bind(&invocation.turn_id)
        .bind(&invocation.tool_call_id)
        .bind(message_id)
        .bind(delivery_id)
        .fetch_one(&mut *tx)
        .await?;
        if !matched {
            return Err(invalid_request(
                "mailbox acknowledgement does not match a claimed delivery",
            ));
        }
        let mut message = read_message(&mut tx, invocation.receiver_thread_id, message_id).await?;
        match message.state {
            MailboxMessageState::Claimed => {
                sqlx::query("UPDATE mailbox_messages SET state = 'consumed' WHERE id = ?")
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await?;
                message.state = MailboxMessageState::Consumed;
                sqlx::query(
                    "UPDATE mailbox_final_subscriptions
                     SET state = 'bound', bound_turn_id = ?
                     WHERE receiver_thread_id = ? AND message_id = ? AND state = 'pending'",
                )
                .bind(&invocation.turn_id)
                .bind(invocation.receiver_thread_id.to_string())
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
            }
            MailboxMessageState::Consumed => {}
            MailboxMessageState::Pending | MailboxMessageState::Rejected => {
                return Err(invalid_request(
                    "mailbox message cannot be acknowledged in its current state",
                ));
            }
        }
        message = read_message(&mut tx, invocation.receiver_thread_id, message_id).await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Rejects pending or claimed mail without deleting content or claim membership.
    ///
    /// The caller owns the policy decision. An identical rejection is idempotent; a consumed
    /// message or a different terminal rejection cannot be rewritten.
    pub async fn reject_mail(
        &self,
        receiver_thread_id: ThreadId,
        message_id: &str,
        reason: &str,
    ) -> anyhow::Result<MailboxMessage> {
        if reason.is_empty() {
            return Err(invalid_request("mailbox rejection reason must be nonempty"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut message = read_message(&mut tx, receiver_thread_id, message_id).await?;
        match message.state {
            MailboxMessageState::Pending | MailboxMessageState::Claimed => {
                sqlx::query(
                    "UPDATE mailbox_messages SET state = 'rejected', rejection_reason = ? WHERE id = ?",
                )
                .bind(reason)
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
                message.state = MailboxMessageState::Rejected;
                message.rejection_reason = Some(reason.to_string());
                sqlx::query(
                    "UPDATE mailbox_final_subscriptions SET state = 'rejected'
                     WHERE receiver_thread_id = ? AND message_id = ? AND state = 'pending'",
                )
                .bind(receiver_thread_id.to_string())
                .bind(message_id)
                .execute(&mut *tx)
                .await?;
            }
            MailboxMessageState::Rejected
                if message.rejection_reason.as_deref() == Some(reason) => {}
            MailboxMessageState::Consumed | MailboxMessageState::Rejected => {
                return Err(invalid_request(
                    "mailbox terminal outcome cannot be rewritten",
                ));
            }
        }
        message = read_message(&mut tx, receiver_thread_id, message_id).await?;
        tx.commit().await?;
        Ok(message)
    }

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
        read_bound_final_subscriptions(&mut *connection, receiver_thread_id, turn_id).await
    }

    /// Reads an accepted message without claiming it or granting delivery authority.
    pub async fn read_mail_by_submission_key(
        &self,
        receiver_thread_id: ThreadId,
        submission_key: &str,
    ) -> anyhow::Result<Option<MailboxMessage>> {
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
        query
            .push(" WHERE m.receiver_thread_id = ")
            .push_bind(receiver_thread_id.to_string())
            .push(" AND m.submission_key = ")
            .push_bind(submission_key);
        query
            .build()
            .fetch_optional(self.pool.as_ref())
            .await?
            .as_ref()
            .map(message_from_row)
            .transpose()
    }

    /// Reads an accepted message without claiming it or granting delivery authority.
    pub async fn read_mail(
        &self,
        receiver_thread_id: ThreadId,
        message_id: &str,
    ) -> anyhow::Result<Option<MailboxMessage>> {
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(MAILBOX_MESSAGE_WITH_SUBSCRIPTION);
        query
            .push(" WHERE m.receiver_thread_id = ")
            .push_bind(receiver_thread_id.to_string())
            .push(" AND m.id = ")
            .push_bind(message_id);
        query
            .build()
            .fetch_optional(self.pool.as_ref())
            .await?
            .as_ref()
            .map(message_from_row)
            .transpose()
    }
}

pub(super) async fn bind_mailbox_final_subscriptions_for_inventory(
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

async fn read_claim(
    connection: &mut SqliteConnection,
    invocation: &MailboxInvocation,
) -> anyhow::Result<Option<MailboxClaim>> {
    let receiver = invocation.receiver_thread_id.to_string();
    let selection: Option<String> = sqlx::query_scalar(
        "SELECT selection_json FROM mailbox_claims
         WHERE receiver_thread_id = ? AND turn_id = ? AND tool_call_id = ?",
    )
    .bind(&receiver)
    .bind(&invocation.turn_id)
    .bind(&invocation.tool_call_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(selection) = selection else {
        return Ok(None);
    };
    let rows = sqlx::query(
        "SELECT m.*, c.delivery_id,
                s.sender_thread_id AS final_subscription_sender_thread_id,
                s.state AS final_subscription_state,
                s.bound_turn_id AS final_subscription_bound_turn_id,
                s.receiver_lifecycle_epoch AS final_subscription_receiver_lifecycle_epoch,
                s.sender_lifecycle_epoch AS final_subscription_sender_lifecycle_epoch
         FROM mailbox_claim_members c
         JOIN mailbox_messages m ON m.id = c.message_id
         LEFT JOIN mailbox_final_subscriptions s
           ON s.receiver_thread_id = m.receiver_thread_id AND s.message_id = m.id
         WHERE c.receiver_thread_id = ? AND c.turn_id = ? AND c.tool_call_id = ?
         ORDER BY m.acceptance_sequence",
    )
    .bind(&receiver)
    .bind(&invocation.turn_id)
    .bind(&invocation.tool_call_id)
    .fetch_all(connection)
    .await?;
    let messages = rows
        .iter()
        .map(|row| {
            Ok(MailboxClaimedMessage {
                message: message_from_row(row)?,
                delivery_id: row.try_get("delivery_id")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Some(MailboxClaim {
        invocation: invocation.clone(),
        selection: serde_json::from_str(&selection)?,
        messages,
    }))
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
