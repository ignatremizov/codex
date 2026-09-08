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
    Ok(MailboxMessage {
        id: row.try_get("id")?,
        receiver_thread_id: ThreadId::try_from(row.try_get::<String, _>("receiver_thread_id")?)?,
        submission_key: row.try_get("submission_key")?,
        sender_key: row.try_get("sender_key")?,
        payload_json: row.try_get("payload_json")?,
        acceptance_sequence: row.try_get("acceptance_sequence")?,
        state,
        rejection_reason: row.try_get("rejection_reason")?,
    })
}

async fn read_message(
    connection: &mut SqliteConnection,
    receiver_thread_id: ThreadId,
    message_id: &str,
) -> anyhow::Result<MailboxMessage> {
    let row = sqlx::query("SELECT * FROM mailbox_messages WHERE receiver_thread_id = ? AND id = ?")
        .bind(receiver_thread_id.to_string())
        .bind(message_id)
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
        if submission_key.is_empty() || sender_key.is_empty() {
            return Err(invalid_request(
                "mailbox submission and sender keys must be nonempty",
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
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
        .await?;
        let row = sqlx::query(
            "SELECT * FROM mailbox_messages WHERE receiver_thread_id = ? AND submission_key = ?",
        )
        .bind(receiver_thread_id.to_string())
        .bind(submission_key)
        .fetch_one(&mut *tx)
        .await?;
        let message = message_from_row(&row)?;
        if message.sender_key != sender_key || message.payload_json != payload_json {
            return Err(invalid_request(
                "mailbox submission key already has different content",
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
        if let MailboxSelection::Senders(senders) = &mut selection {
            if senders.iter().any(String::is_empty) {
                return Err(invalid_request("mailbox sender keys must be nonempty"));
            }
            senders.sort();
            senders.dedup();
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
             SELECT m.*, c.delivery_id, c.turn_id, c.tool_call_id, claims.selection_json
             FROM outstanding o
             JOIN mailbox_claims claims
               USING (receiver_thread_id, turn_id, tool_call_id)
             JOIN mailbox_claim_members c
               USING (receiver_thread_id, turn_id, tool_call_id)
             JOIN mailbox_messages m ON m.id = c.message_id
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
            }
            MailboxMessageState::Consumed => {}
            MailboxMessageState::Pending | MailboxMessageState::Rejected => {
                return Err(invalid_request(
                    "mailbox message cannot be acknowledged in its current state",
                ));
            }
        }
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
            }
            MailboxMessageState::Rejected
                if message.rejection_reason.as_deref() == Some(reason) => {}
            MailboxMessageState::Consumed | MailboxMessageState::Rejected => {
                return Err(invalid_request(
                    "mailbox terminal outcome cannot be rewritten",
                ));
            }
        }
        tx.commit().await?;
        Ok(message)
    }

    /// Reads an accepted message without claiming it or granting delivery authority.
    pub async fn read_mail_by_submission_key(
        &self,
        receiver_thread_id: ThreadId,
        submission_key: &str,
    ) -> anyhow::Result<Option<MailboxMessage>> {
        sqlx::query(
            "SELECT * FROM mailbox_messages WHERE receiver_thread_id = ? AND submission_key = ?",
        )
        .bind(receiver_thread_id.to_string())
        .bind(submission_key)
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
        sqlx::query("SELECT * FROM mailbox_messages WHERE receiver_thread_id = ? AND id = ?")
            .bind(receiver_thread_id.to_string())
            .bind(message_id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .as_ref()
            .map(message_from_row)
            .transpose()
    }
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
        "SELECT m.*, c.delivery_id FROM mailbox_claim_members c
         JOIN mailbox_messages m ON m.id = c.message_id
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
