use super::StateRuntime;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;

/// Result of an owned lifecycle update, distinct from best-effort queue cleanup.
///
/// Only `Revoked` permits retiring final subscriptions for this transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLifecycleAuthorityUpdate {
    NotOwned,
    Unchanged,
    Revoked,
}

/// Captured ownership of one restoration edge, checked in the closing graph transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSpawnEdgeAuthority {
    pub thread_id: ThreadId,
    pub parent_thread_id: ThreadId,
    pub owner_session_id: Option<SessionId>,
}

impl StateRuntime {
    /// Closes only the captured edge and owner, rejecting stale restoration cleanup.
    pub async fn close_thread_spawn_edge_if_current(
        &self,
        expected: ThreadSpawnEdgeAuthority,
        revoked_thread_ids: &[ThreadId],
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        super::agent_aliases::require_not_deleted(&mut tx, expected.thread_id).await?;
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT session_id FROM agent_aliases
             WHERE thread_id = ? AND ownership_state = 'current'",
        )
        .bind(expected.thread_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        anyhow::ensure!(
            owner == expected.owner_session_id.map(|id| id.to_string()),
            "restoration edge owner changed"
        );
        let edge = sqlx::query(
            "SELECT parent_thread_id, status FROM thread_spawn_edges WHERE child_thread_id = ?",
        )
        .bind(expected.thread_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("restoration edge is missing"))?;
        anyhow::ensure!(
            edge.try_get::<String, _>("parent_thread_id")? == expected.parent_thread_id.to_string(),
            "restoration edge parent changed"
        );
        let status: String = edge.try_get("status")?;
        let revoked = match status.as_str() {
            "open" => true,
            "closed" => false,
            _ => anyhow::bail!("invalid restoration edge status"),
        };
        if revoked {
            anyhow::ensure!(
                revoked_thread_ids.contains(&expected.thread_id),
                "revoked subtree must contain its closed agent"
            );
            super::threads::set_thread_spawn_edge_status_in_transaction(
                &mut tx,
                expected.thread_id,
                crate::DirectionalThreadSpawnEdgeStatus::Closed,
            )
            .await?;
            advance_agent_thread_lifecycle_authority_epochs_in_transaction(
                &mut tx,
                revoked_thread_ids,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(revoked)
    }

    /// Read persistent lifecycle authority epochs, defaulting new threads to epoch zero.
    pub async fn read_agent_thread_lifecycle_epochs(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<Vec<(ThreadId, i64)>> {
        if thread_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT thread_id, epoch FROM agent_thread_lifecycle_authority_epochs WHERE thread_id IN (",
        );
        let mut separated = builder.separated(", ");
        for thread_id in thread_ids {
            separated.push_bind(thread_id.to_string());
        }
        separated.push_unseparated(")");
        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        let epochs = rows
            .into_iter()
            .map(|row| {
                Ok((
                    ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
                    row.try_get::<i64, _>("epoch")?,
                ))
            })
            .collect::<anyhow::Result<std::collections::HashMap<_, _>>>()?;
        thread_ids
            .iter()
            .copied()
            .map(|thread_id| {
                let epoch = epochs.get(&thread_id).copied().unwrap_or_default();
                Ok((thread_id, epoch))
            })
            .collect()
    }
}

pub(super) async fn advance_agent_thread_lifecycle_authority_epochs_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    thread_ids: &[ThreadId],
) -> anyhow::Result<()> {
    let mut thread_ids = thread_ids.to_vec();
    thread_ids.sort_by_key(ToString::to_string);
    thread_ids.dedup();
    for thread_id in thread_ids {
        let advanced = sqlx::query(
            "INSERT INTO agent_thread_lifecycle_authority_epochs (thread_id, epoch)
             VALUES (?, 1)
             ON CONFLICT(thread_id) DO UPDATE SET epoch = epoch + 1
             WHERE typeof(epoch) = 'integer' AND epoch < 9223372036854775807",
        )
        .bind(thread_id.to_string())
        .execute(&mut **tx)
        .await?
        .rows_affected();
        anyhow::ensure!(
            advanced == 1,
            "lifecycle authority epoch exhausted for {thread_id}"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "lifecycle_authority_tests.rs"]
mod tests;
