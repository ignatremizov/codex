//! Promote one exclusively reserved subtree; historical aliases remain in the old namespace.

use super::super::StateRuntime;
use super::AgentAliasOwnershipState;
use super::namespace::ensure_agent_alias_namespace_in_transaction;
use super::queries::find_agent_alias_by_thread_in_transaction;
use super::queries::find_current_agent_alias_by_thread_in_transaction;
use super::queries::find_spawn_parent_in_transaction;
use super::transfer::activate_transferred_member;
use super::transfer::load_transfer_members;
use super::transfer::tombstone_previous_aliases;
use anyhow::Context;
use chrono::Utc;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::SessionId;
use std::collections::HashMap;

impl StateRuntime {
    /// Compare owner and complete subtree before atomically removing its incoming spawn edge.
    pub async fn detach_agent_alias(
        &self,
        request: crate::AgentAliasDetachRequest,
    ) -> anyhow::Result<crate::AgentAliasTransfer> {
        let crate::AgentAliasDetachRequest {
            thread_id,
            expected_previous_session_id,
            mut expected_descendant_thread_ids,
        } = request;
        let new_session_id = SessionId::from(thread_id);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        super::require_not_deleted(&mut tx, thread_id).await?;
        let owner = find_current_agent_alias_by_thread_in_transaction(&mut tx, thread_id)
            .await?
            .context("detach requires a current child owner")?;
        if owner.session_id != expected_previous_session_id || owner.agent_ref == 1 {
            anyhow::bail!("agent {thread_id} is no longer owned by the expected parent root");
        }
        let parent = find_spawn_parent_in_transaction(&mut tx, thread_id)
            .await?
            .context("detach requires a current parent edge")?;
        let mut members = load_transfer_members(
            &mut tx,
            thread_id,
            Some(parent),
            /*requested_nickname*/ None,
            Some(expected_previous_session_id),
        )
        .await?;
        expected_descendant_thread_ids.sort_by_key(ToString::to_string);
        expected_descendant_thread_ids.dedup();
        let mut actual_descendants = members
            .iter()
            .skip(1)
            .map(|member| member.thread_id)
            .collect::<Vec<_>>();
        actual_descendants.sort_by_key(ToString::to_string);
        if actual_descendants != expected_descendant_thread_ids {
            anyhow::bail!("agent {thread_id} subtree changed before detach");
        }
        // A previously promoted root may retain transferred aliases and monotonic refs.
        // Its old namespace must not contain unrelated current owners.
        let occupied = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM agent_aliases WHERE session_id = ? AND ownership_state = 'current'",
        )
        .bind(new_session_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if occupied != 0 {
            anyhow::bail!("the detached root namespace already has current owners");
        }
        tombstone_previous_aliases(&mut tx, &members).await?;
        sqlx::query("DELETE FROM thread_spawn_edges WHERE child_thread_id = ?")
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        // Suppress legacy topology backfill: the complete reserved membership is installed below.
        sqlx::query(
            "INSERT INTO agent_alias_namespaces (session_id, next_agent_ref) VALUES (?, 2) \
             ON CONFLICT(session_id) DO NOTHING",
        )
        .bind(new_session_id.to_string())
        .execute(&mut *tx)
        .await?;
        if let Some(existing) =
            find_agent_alias_by_thread_in_transaction(&mut tx, new_session_id, thread_id).await?
        {
            if existing.agent_ref != 1 || existing.state != crate::AgentAliasState::Transferred {
                anyhow::bail!("detached root has an inconsistent reserved Main identity");
            }
            sqlx::query(
                "UPDATE agent_aliases SET ownership_state = ?, nickname = ?, task_path = '/root' \
                 WHERE session_id = ? AND thread_id = ? AND agent_ref = 1",
            )
            .bind(AgentAliasOwnershipState::Current.as_ref())
            .bind(MAIN_AGENT_NICKNAME)
            .bind(new_session_id.to_string())
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        let alias = ensure_agent_alias_namespace_in_transaction(&mut tx, new_session_id).await?;
        let previous_path = members
            .first()
            .context("detached subtree omitted its selected child")?
            .task_path
            .clone();
        let mut task_path_mapping = Vec::new();
        if previous_path.as_deref() != Some("/root") {
            task_path_mapping.push(crate::AgentTaskPathMapping {
                thread_id,
                previous_task_path: previous_path.clone(),
                task_path: Some("/root".into()),
            });
        }
        let original_paths = members
            .iter()
            .skip(1)
            .map(|member| (member.thread_id, member.task_path.clone()))
            .collect::<HashMap<_, _>>();
        for member in members.iter_mut().skip(1) {
            if let Some(suffix) = previous_path
                .as_deref()
                .and_then(|prefix| member.task_path.as_deref()?.strip_prefix(prefix))
                .filter(|suffix| suffix.starts_with('/'))
            {
                member.task_path = Some(format!("/root{suffix}"));
            }
        }
        let collisions = super::task_paths::resolve_imported_task_paths(
            &mut tx,
            new_session_id,
            thread_id,
            /*requested_task_path*/ None,
            members
                .iter()
                .skip(1)
                .map(|member| (member.thread_id, member.task_path.clone()))
                .collect(),
        )
        .await?
        .into_iter()
        .map(|mapping| (mapping.thread_id, mapping.task_path))
        .collect::<HashMap<_, _>>();
        for member in members.iter_mut().skip(1) {
            if let Some(path) = collisions.get(&member.thread_id) {
                member.task_path = path.clone();
            }
            let previous_task_path = original_paths
                .get(&member.thread_id)
                .context("detached descendant omitted its original task path")?
                .clone();
            if member.task_path != previous_task_path {
                task_path_mapping.push(crate::AgentTaskPathMapping {
                    thread_id: member.thread_id,
                    previous_task_path,
                    task_path: member.task_path.clone(),
                });
            }
            activate_transferred_member(&mut tx, new_session_id, member).await?;
        }
        task_path_mapping
            .sort_by(|left, right| left.previous_task_path.cmp(&right.previous_task_path));
        let detached_at_ms = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO agent_root_detachments \
             (thread_id, previous_session_id, previous_parent_thread_id, detached_at_ms) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(thread_id.to_string())
        .bind(expected_previous_session_id.to_string())
        .bind(parent.to_string())
        .bind(detached_at_ms)
        .execute(&mut *tx)
        .await?;
        super::advance_agent_thread_lifecycle_authority_epochs_in_transaction(
            &mut tx,
            &std::iter::once(thread_id)
                .chain(expected_descendant_thread_ids)
                .collect::<Vec<_>>(),
        )
        .await?;
        tx.commit().await?;
        Ok(crate::AgentAliasTransfer::Transferred {
            alias,
            previous_session_id: Some(expected_previous_session_id),
            previous_parent_thread_id: Some(parent),
            transferred_at_ms: detached_at_ms,
            task_path_mapping,
        })
    }
}

#[cfg(test)]
#[path = "detach_tests.rs"]
mod tests;
