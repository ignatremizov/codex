use anyhow::Context;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::sqlite::SqliteRow;

use super::super::StateRuntime;
use super::AgentAliasOwnershipState;

impl StateRuntime {
    /// Find one alias by canonical thread UUID.
    pub async fn find_agent_alias_by_thread(
        &self,
        session_id: SessionId,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<crate::AgentAliasRecord>> {
        let row = sqlx::query(
            r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ? AND alias.thread_id = ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(agent_alias_from_row).transpose()
    }

    /// Find the active or closed alias that currently owns a canonical thread UUID.
    pub async fn find_current_agent_alias_by_thread(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<crate::AgentAliasRecord>> {
        let row = sqlx::query(
            r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.thread_id = ?
  AND alias.ownership_state = ?
            "#,
        )
        .bind(thread_id.to_string())
        .bind(AgentAliasOwnershipState::Current.as_ref())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(agent_alias_from_row).transpose()
    }

    /// Find one alias by root-scoped numeric ref.
    pub async fn find_agent_alias_by_ref(
        &self,
        session_id: SessionId,
        agent_ref: u64,
    ) -> anyhow::Result<Option<crate::AgentAliasRecord>> {
        let agent_ref =
            i64::try_from(agent_ref).context("agent ref exceeds SQLite integer range")?;
        let row = sqlx::query(
            r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ? AND alias.agent_ref = ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(agent_ref)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(agent_alias_from_row).transpose()
    }

    /// Find one alias by root-scoped nickname.
    ///
    /// Ordinary nicknames match exactly. The reserved Main nickname is case-insensitive.
    pub async fn find_agent_alias_by_nickname(
        &self,
        session_id: SessionId,
        nickname: &str,
    ) -> anyhow::Result<Option<crate::AgentAliasRecord>> {
        let nickname = if nickname.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME) {
            MAIN_AGENT_NICKNAME
        } else {
            nickname
        };
        let row = sqlx::query(
            r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ? AND alias.nickname = ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(nickname)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(agent_alias_from_row).transpose()
    }

    /// List every alias in stable numeric-ref order, including closed or transferred reservations.
    pub async fn list_agent_aliases(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Vec<crate::AgentAliasRecord>> {
        let rows = sqlx::query(
            r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ?
ORDER BY alias.agent_ref
            "#,
        )
        .bind(session_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter().map(agent_alias_from_row).collect()
    }

    /// List inherited nickname reservations that intentionally do not resolve.
    pub async fn list_agent_nickname_reservations(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT nickname FROM agent_alias_nickname_reservations WHERE session_id = ? ORDER BY nickname",
        )
        .bind(session_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(Into::into)
    }
}

pub(super) async fn find_agent_alias_by_thread_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    thread_id: ThreadId,
) -> anyhow::Result<Option<crate::AgentAliasRecord>> {
    let row = sqlx::query(
        r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ? AND alias.thread_id = ?
        "#,
    )
    .bind(session_id.to_string())
    .bind(thread_id.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(agent_alias_from_row).transpose()
}

pub(super) async fn find_current_agent_alias_by_thread_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    thread_id: ThreadId,
) -> anyhow::Result<Option<crate::AgentAliasRecord>> {
    let row = sqlx::query(
        r#"
SELECT
    alias.session_id,
    alias.thread_id,
    alias.agent_ref,
    alias.nickname,
    alias.ownership_state,
    edge.status AS edge_status
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.thread_id = ?
  AND alias.ownership_state = ?
        "#,
    )
    .bind(thread_id.to_string())
    .bind(AgentAliasOwnershipState::Current.as_ref())
    .fetch_optional(&mut **tx)
    .await?;
    row.map(agent_alias_from_row).transpose()
}

pub(super) async fn require_active_parent_alias(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    parent_thread_id: ThreadId,
) -> anyhow::Result<()> {
    let parent_is_active = sqlx::query_scalar::<_, i64>(
        r#"
SELECT 1
FROM agent_aliases AS alias
LEFT JOIN thread_spawn_edges AS edge ON edge.child_thread_id = alias.thread_id
WHERE alias.session_id = ?
  AND alias.thread_id = ?
  AND alias.ownership_state = ?
  AND (
      alias.thread_id = alias.session_id
      OR edge.status = ?
  )
        "#,
    )
    .bind(session_id.to_string())
    .bind(parent_thread_id.to_string())
    .bind(AgentAliasOwnershipState::Current.as_ref())
    .bind(crate::DirectionalThreadSpawnEdgeStatus::Open.as_ref())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if !parent_is_active {
        anyhow::bail!(
            "parent thread {parent_thread_id} has no active alias in session {session_id}"
        );
    }
    Ok(())
}

pub(super) async fn find_spawn_parent_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    thread_id: ThreadId,
) -> anyhow::Result<Option<ThreadId>> {
    sqlx::query_scalar::<_, String>(
        "SELECT parent_thread_id FROM thread_spawn_edges WHERE child_thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .map(ThreadId::try_from)
    .transpose()
    .map_err(Into::into)
}

fn agent_alias_from_row(row: SqliteRow) -> anyhow::Result<crate::AgentAliasRecord> {
    let agent_ref = row.try_get::<i64, _>("agent_ref")?;
    let session_id = SessionId::try_from(row.try_get::<String, _>("session_id")?)?;
    let thread_id = ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?;
    let ownership_state = row
        .try_get::<String, _>("ownership_state")?
        .parse::<AgentAliasOwnershipState>()?;
    let state = match ownership_state {
        AgentAliasOwnershipState::Transferred => crate::AgentAliasState::Transferred,
        AgentAliasOwnershipState::Current if thread_id == ThreadId::from(session_id) => {
            crate::AgentAliasState::Active
        }
        AgentAliasOwnershipState::Current => {
            match row
                .try_get::<Option<String>, _>("edge_status")?
                .context("current child alias has no persisted spawn edge")?
                .parse::<crate::DirectionalThreadSpawnEdgeStatus>()?
            {
                crate::DirectionalThreadSpawnEdgeStatus::Open => crate::AgentAliasState::Active,
                crate::DirectionalThreadSpawnEdgeStatus::Closed => crate::AgentAliasState::Closed,
            }
        }
    };
    Ok(crate::AgentAliasRecord {
        session_id,
        thread_id,
        agent_ref: u64::try_from(agent_ref).context("stored agent ref is negative")?,
        nickname: row.try_get("nickname")?,
        state,
    })
}
