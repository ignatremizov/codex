use anyhow::Context;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use tracing::warn;

use super::queries::find_agent_alias_by_thread_in_transaction;

pub(super) async fn ensure_agent_alias_namespace_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
) -> anyhow::Result<crate::AgentAliasRecord> {
    let namespace_created = sqlx::query(
        r#"
INSERT INTO agent_alias_namespaces (
    session_id,
    next_agent_ref
) VALUES (?, 2)
ON CONFLICT(session_id) DO NOTHING
        "#,
    )
    .bind(session_id.to_string())
    .execute(&mut **tx)
    .await?
    .rows_affected()
        != 0;

    let root_thread_id = ThreadId::from(session_id);
    sqlx::query(
        r#"
INSERT INTO agent_aliases (
    session_id,
    thread_id,
    agent_ref,
    nickname,
    ownership_state
) VALUES (?, ?, 1, ?, ?)
ON CONFLICT(session_id, thread_id) DO NOTHING
        "#,
    )
    .bind(session_id.to_string())
    .bind(root_thread_id.to_string())
    .bind(MAIN_AGENT_NICKNAME)
    .bind(super::AgentAliasOwnershipState::Current.as_ref())
    .execute(&mut **tx)
    .await?;

    if namespace_created {
        // Existing topology can predate migration 0048. Once the namespace exists, bound control
        // paths persist aliases and edges atomically, so routine ensures do not repeat this scan.
        backfill_agent_aliases_in_transaction(tx, session_id, root_thread_id).await?;
    }

    find_agent_alias_by_thread_in_transaction(tx, session_id, root_thread_id)
        .await?
        .context("agent alias namespace has no Main alias")
}

async fn backfill_agent_aliases_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    root_thread_id: ThreadId,
) -> anyhow::Result<()> {
    let rows = sqlx::query(
        r#"
WITH RECURSIVE subtree(child_thread_id, depth, status, visited, has_cycle) AS (
    SELECT
        child_thread_id,
        1,
        status,
        ',' || ? || ',' || child_thread_id || ',',
        0
    FROM thread_spawn_edges
    WHERE parent_thread_id = ?
    UNION ALL
    SELECT
        edge.child_thread_id,
        subtree.depth + 1,
        edge.status,
        subtree.visited || edge.child_thread_id || ',',
        INSTR(subtree.visited, ',' || edge.child_thread_id || ',') > 0
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
    WHERE subtree.has_cycle = 0
)
SELECT
    subtree.child_thread_id,
    subtree.status,
    subtree.has_cycle,
    threads.agent_nickname
FROM subtree
LEFT JOIN threads ON threads.id = subtree.child_thread_id
ORDER BY subtree.depth, subtree.child_thread_id
        "#,
    )
    .bind(root_thread_id.to_string())
    .bind(root_thread_id.to_string())
    .fetch_all(&mut **tx)
    .await?;

    for row in rows {
        let thread_id = ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?)?;
        if thread_id == root_thread_id || row.try_get::<i64, _>("has_cycle")? != 0 {
            anyhow::bail!(
                "cannot backfill aliases for {root_thread_id}: persisted spawn graph is cyclic at {thread_id}"
            );
        }
        if find_agent_alias_by_thread_in_transaction(tx, session_id, thread_id)
            .await?
            .is_some()
        {
            continue;
        }
        let state = match row
            .try_get::<String, _>("status")?
            .parse::<crate::DirectionalThreadSpawnEdgeStatus>()?
        {
            crate::DirectionalThreadSpawnEdgeStatus::Open => crate::AgentAliasState::Active,
            crate::DirectionalThreadSpawnEdgeStatus::Closed => crate::AgentAliasState::Closed,
        };
        let stored_nickname = row.try_get::<Option<String>, _>("agent_nickname")?;
        let nickname = if let Some(nickname) = stored_nickname.as_deref() {
            if is_main_agent_nickname(nickname) {
                warn!(
                    %session_id,
                    %thread_id,
                    nickname,
                    "omitting reserved Main nickname while backfilling durable agent aliases"
                );
                None
            } else {
                let reserved = sqlx::query_scalar::<_, i64>(
                    "SELECT 1 FROM agent_aliases WHERE session_id = ? AND nickname = ?",
                )
                .bind(session_id.to_string())
                .bind(nickname)
                .fetch_optional(&mut **tx)
                .await?
                .is_some();
                if reserved {
                    warn!(
                        %session_id,
                        %thread_id,
                        nickname,
                        "omitting duplicate nickname while backfilling durable agent aliases"
                    );
                    None
                } else {
                    Some(nickname)
                }
            }
        } else {
            None
        };
        insert_agent_alias_in_transaction(tx, session_id, thread_id, nickname, state).await?;
    }
    Ok(())
}

pub(super) async fn insert_agent_alias_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    thread_id: ThreadId,
    nickname: Option<&str>,
    state: crate::AgentAliasState,
) -> anyhow::Result<crate::AgentAliasRecord> {
    if nickname.is_some_and(is_main_agent_nickname) {
        anyhow::bail!("agent nickname {MAIN_AGENT_NICKNAME:?} is reserved for the root thread");
    }
    if state == crate::AgentAliasState::Transferred {
        anyhow::bail!("new agent aliases must be inserted under current ownership");
    }
    if let Some(nickname) = nickname
        && let Some(existing_thread_id) = sqlx::query_scalar::<_, String>(
            "SELECT thread_id FROM agent_aliases WHERE session_id = ? AND nickname = ?",
        )
        .bind(session_id.to_string())
        .bind(nickname)
        .fetch_optional(&mut **tx)
        .await?
    {
        let existing_thread_id = ThreadId::try_from(existing_thread_id)?;
        anyhow::bail!(
            "agent nickname {nickname:?} is already reserved by thread {existing_thread_id}; retry the spawn to choose another nickname"
        );
    }
    release_matching_nickname_reservation_in_transaction(tx, session_id, thread_id, nickname)
        .await?;
    let next_agent_ref = sqlx::query_scalar::<_, i64>(
        "SELECT next_agent_ref FROM agent_alias_namespaces WHERE session_id = ?",
    )
    .bind(session_id.to_string())
    .fetch_one(&mut **tx)
    .await?;
    let following_agent_ref = next_agent_ref
        .checked_add(1)
        .context("agent ref high-water mark overflowed")?;

    sqlx::query(
        r#"
INSERT INTO agent_aliases (
    session_id,
    thread_id,
    agent_ref,
    nickname,
    ownership_state
) VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(session_id.to_string())
    .bind(thread_id.to_string())
    .bind(next_agent_ref)
    .bind(nickname)
    .bind(super::AgentAliasOwnershipState::Current.as_ref())
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE agent_alias_namespaces SET next_agent_ref = ? WHERE session_id = ?")
        .bind(following_agent_ref)
        .bind(session_id.to_string())
        .execute(&mut **tx)
        .await?;

    Ok(crate::AgentAliasRecord {
        session_id,
        thread_id,
        agent_ref: u64::try_from(next_agent_ref).context("stored agent ref is negative")?,
        nickname: nickname.map(ToString::to_string),
        state,
    })
}

pub(super) fn is_main_agent_nickname(nickname: &str) -> bool {
    nickname.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME)
}

pub(super) async fn release_matching_nickname_reservation_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    thread_id: ThreadId,
    nickname: Option<&str>,
) -> anyhow::Result<()> {
    let Some(nickname) = nickname else {
        return Ok(());
    };
    let reserved_thread_id = sqlx::query_scalar::<_, String>(
        "SELECT source_thread_id FROM agent_alias_nickname_reservations WHERE session_id = ? AND nickname = ?",
    )
    .bind(session_id.to_string())
    .bind(nickname)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(reserved_thread_id) = reserved_thread_id else {
        return Ok(());
    };
    let reserved_thread_id = ThreadId::try_from(reserved_thread_id)?;
    if reserved_thread_id != thread_id {
        anyhow::bail!(
            "agent nickname {nickname:?} is reserved for inherited thread {reserved_thread_id}"
        );
    }
    sqlx::query(
        "DELETE FROM agent_alias_nickname_reservations WHERE session_id = ? AND nickname = ?",
    )
    .bind(session_id.to_string())
    .bind(nickname)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
