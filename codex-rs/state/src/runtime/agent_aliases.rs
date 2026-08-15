use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use strum::AsRefStr;
use strum::EnumString;

use super::StateRuntime;
use super::threads::set_thread_spawn_edge_status_in_transaction;
use super::threads::upsert_thread_spawn_edge_in_transaction;
use namespace::ensure_agent_alias_namespace_in_transaction;
use namespace::insert_agent_alias_in_transaction;
use namespace::is_main_agent_nickname;
use namespace::release_matching_nickname_reservation_in_transaction;
use queries::find_agent_alias_by_thread_in_transaction;
use queries::find_spawn_parent_in_transaction;
use queries::require_active_parent_alias;

mod namespace;
mod queries;
mod transfer;

#[derive(Clone, Copy, Debug, Eq, PartialEq, AsRefStr, EnumString)]
#[strum(serialize_all = "snake_case")]
enum AgentAliasOwnershipState {
    Current,
    Transferred,
}

impl StateRuntime {
    /// Ensure that a root session has its durable namespace and ref-1 Main alias.
    pub async fn ensure_agent_alias_namespace(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<crate::AgentAliasRecord> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let root_alias = ensure_agent_alias_namespace_in_transaction(&mut tx, session_id).await?;
        tx.commit().await?;
        Ok(root_alias)
    }

    /// Reserve inherited aliases in a new root without copying live target mappings.
    pub async fn reserve_agent_aliases_for_fork(
        &self,
        request: crate::AgentAliasForkReservation,
    ) -> anyhow::Result<()> {
        let crate::AgentAliasForkReservation {
            source_session_id,
            fork_session_id,
        } = request;
        if source_session_id == fork_session_id {
            anyhow::bail!("a root cannot import alias reservations from itself");
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_agent_alias_namespace_in_transaction(&mut tx, source_session_id).await?;
        ensure_agent_alias_namespace_in_transaction(&mut tx, fork_session_id).await?;

        let source_next_agent_ref = sqlx::query_scalar::<_, i64>(
            "SELECT next_agent_ref FROM agent_alias_namespaces WHERE session_id = ?",
        )
        .bind(source_session_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE agent_alias_namespaces SET next_agent_ref = MAX(next_agent_ref, ?) WHERE session_id = ?",
        )
        .bind(source_next_agent_ref)
        .bind(fork_session_id.to_string())
        .execute(&mut *tx)
        .await?;

        let inherited = sqlx::query_as::<_, (String, String)>(
            r#"
SELECT nickname, thread_id
FROM agent_aliases
WHERE session_id = ? AND nickname IS NOT NULL
UNION
SELECT nickname, source_thread_id
FROM agent_alias_nickname_reservations
WHERE session_id = ?
ORDER BY nickname
            "#,
        )
        .bind(source_session_id.to_string())
        .bind(source_session_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        for (nickname, source_thread_id) in inherited {
            // Every fork owns a distinct ref-1 Main alias. The source root's reserved nickname is
            // identity, not inherited child history.
            if is_main_agent_nickname(&nickname) {
                continue;
            }
            let existing_alias_thread = sqlx::query_scalar::<_, String>(
                "SELECT thread_id FROM agent_aliases WHERE session_id = ? AND nickname = ?",
            )
            .bind(fork_session_id.to_string())
            .bind(&nickname)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing_alias_thread) = existing_alias_thread.as_deref()
                && existing_alias_thread != source_thread_id.as_str()
            {
                anyhow::bail!(
                    "fork alias reservation {nickname:?} conflicts with target thread {existing_alias_thread}"
                );
            }
            if existing_alias_thread.is_some() {
                continue;
            }

            let existing_reservation_thread = sqlx::query_scalar::<_, String>(
                "SELECT source_thread_id FROM agent_alias_nickname_reservations WHERE session_id = ? AND nickname = ?",
            )
            .bind(fork_session_id.to_string())
            .bind(&nickname)
            .fetch_optional(&mut *tx)
            .await?;
            match existing_reservation_thread {
                Some(existing_reservation_thread)
                    if existing_reservation_thread != source_thread_id =>
                {
                    anyhow::bail!(
                        "fork nickname reservation {nickname:?} already belongs to {existing_reservation_thread}"
                    );
                }
                Some(_) => {}
                None => {
                    sqlx::query(
                        "INSERT INTO agent_alias_nickname_reservations (session_id, nickname, source_thread_id) VALUES (?, ?, ?)",
                    )
                    .bind(fork_session_id.to_string())
                    .bind(nickname)
                    .bind(source_thread_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        tx.commit().await?;
        Ok(())
    }

    /// Delete reservation-only state for a fork that never became externally visible.
    pub async fn discard_fork_agent_alias_reservations(
        &self,
        fork_session_id: SessionId,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let fork_session_id = fork_session_id.to_string();
        let deleted = sqlx::query(
            r#"
DELETE FROM agent_alias_namespaces
WHERE session_id = ?
  AND NOT EXISTS (
      SELECT 1
      FROM agent_aliases
      WHERE session_id = ?
        AND agent_ref <> 1
  )
  AND NOT EXISTS (
      SELECT 1
      FROM agent_alias_transfers
      WHERE new_session_id = ?
  )
            "#,
        )
        .bind(&fork_session_id)
        .bind(&fork_session_id)
        .bind(&fork_session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            != 0;
        tx.commit().await?;
        Ok(deleted)
    }

    /// Atomically allocate a child alias and persist its current parent edge.
    pub async fn allocate_agent_alias(
        &self,
        allocation: crate::AgentAliasAllocation,
    ) -> anyhow::Result<crate::AgentAliasRecord> {
        let crate::AgentAliasAllocation {
            session_id,
            parent_thread_id,
            child_thread_id,
            nickname,
        } = allocation;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_agent_alias_namespace_in_transaction(&mut tx, session_id).await?;

        let root_thread_id = ThreadId::from(session_id);
        if child_thread_id == root_thread_id {
            anyhow::bail!("Main already owns ref 1 and cannot be allocated as a child alias");
        }
        if child_thread_id == parent_thread_id {
            anyhow::bail!("an agent alias cannot be its own parent");
        }
        if nickname.as_deref().is_some_and(str::is_empty) {
            anyhow::bail!("an agent nickname cannot be empty");
        }
        if nickname.as_deref().is_some_and(is_main_agent_nickname) {
            anyhow::bail!("Main is reserved for the root thread");
        }
        require_active_parent_alias(&mut tx, session_id, parent_thread_id).await?;

        if let Some(existing) =
            find_agent_alias_by_thread_in_transaction(&mut tx, session_id, child_thread_id).await?
        {
            match existing.state {
                crate::AgentAliasState::Active => {
                    tx.commit().await?;
                    return Ok(existing);
                }
                crate::AgentAliasState::Closed => {
                    anyhow::bail!(
                        "agent {child_thread_id} already has a closed alias; activate it instead"
                    );
                }
                crate::AgentAliasState::Transferred => {
                    anyhow::bail!(
                        "agent {child_thread_id} was transferred out of session {session_id}"
                    );
                }
            }
        }

        let alias = insert_agent_alias_in_transaction(
            &mut tx,
            session_id,
            child_thread_id,
            nickname.as_deref(),
            crate::AgentAliasState::Active,
        )
        .await?;
        upsert_thread_spawn_edge_in_transaction(
            &mut tx,
            parent_thread_id,
            child_thread_id,
            crate::DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await?;

        tx.commit().await?;
        Ok(alias)
    }

    /// Reopen a closed alias and its existing parent edge, or allocate it when absent.
    pub async fn activate_agent_alias(
        &self,
        allocation: crate::AgentAliasAllocation,
    ) -> anyhow::Result<crate::AgentAliasRecord> {
        let crate::AgentAliasAllocation {
            session_id,
            parent_thread_id,
            child_thread_id,
            nickname,
        } = allocation;
        let root_thread_id = ThreadId::from(session_id);
        if child_thread_id == root_thread_id {
            anyhow::bail!("Main's root alias is always active");
        }
        if child_thread_id == parent_thread_id {
            anyhow::bail!("an agent alias cannot be its own parent");
        }
        if nickname.as_deref().is_some_and(str::is_empty) {
            anyhow::bail!("an agent nickname cannot be empty");
        }
        if nickname.as_deref().is_some_and(is_main_agent_nickname) {
            anyhow::bail!("Main is reserved for the root thread");
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_agent_alias_namespace_in_transaction(&mut tx, session_id).await?;
        require_active_parent_alias(&mut tx, session_id, parent_thread_id).await?;

        let mut alias =
            match find_agent_alias_by_thread_in_transaction(&mut tx, session_id, child_thread_id)
                .await?
            {
                Some(alias) => match alias.state {
                    crate::AgentAliasState::Active => alias,
                    crate::AgentAliasState::Closed => {
                        release_matching_nickname_reservation_in_transaction(
                            &mut tx,
                            session_id,
                            child_thread_id,
                            alias.nickname.as_deref(),
                        )
                        .await?;
                        crate::AgentAliasRecord {
                            state: crate::AgentAliasState::Active,
                            ..alias
                        }
                    }
                    crate::AgentAliasState::Transferred => {
                        anyhow::bail!(
                            "agent {child_thread_id} was transferred out of session {session_id}"
                        );
                    }
                },
                None => {
                    insert_agent_alias_in_transaction(
                        &mut tx,
                        session_id,
                        child_thread_id,
                        nickname.as_deref(),
                        crate::AgentAliasState::Active,
                    )
                    .await?
                }
            };
        if alias.nickname.is_none()
            && let Some(nickname) = nickname.as_deref()
        {
            release_matching_nickname_reservation_in_transaction(
                &mut tx,
                session_id,
                child_thread_id,
                Some(nickname),
            )
            .await?;
            sqlx::query(
                "UPDATE agent_aliases SET nickname = ? WHERE session_id = ? AND thread_id = ?",
            )
            .bind(nickname)
            .bind(session_id.to_string())
            .bind(child_thread_id.to_string())
            .execute(&mut *tx)
            .await?;
            alias.nickname = Some(nickname.to_string());
        }
        if find_spawn_parent_in_transaction(&mut tx, child_thread_id)
            .await?
            .is_some()
        {
            set_thread_spawn_edge_status_in_transaction(
                &mut tx,
                child_thread_id,
                crate::DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;
        } else {
            upsert_thread_spawn_edge_in_transaction(
                &mut tx,
                parent_thread_id,
                child_thread_id,
                crate::DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(alias)
    }

    /// Update an owned agent's lifecycle edge without changing its reserved ref or nickname.
    pub async fn set_agent_lifecycle_state(
        &self,
        session_id: SessionId,
        thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<bool> {
        if thread_id == ThreadId::from(session_id) {
            anyhow::bail!("Main's root alias lifecycle cannot be changed");
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owns_alias = sqlx::query_scalar::<_, i64>(
            r#"
SELECT 1
FROM agent_aliases
WHERE session_id = ?
  AND thread_id = ?
  AND ownership_state = ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(thread_id.to_string())
        .bind(AgentAliasOwnershipState::Current.as_ref())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !owns_alias {
            tx.commit().await?;
            return Ok(false);
        }
        set_thread_spawn_edge_status_in_transaction(&mut tx, thread_id, status).await?;
        tx.commit().await?;
        Ok(true)
    }
}
