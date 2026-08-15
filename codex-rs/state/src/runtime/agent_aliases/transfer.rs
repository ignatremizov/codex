use anyhow::Context;
use chrono::Utc;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use sqlx::Acquire;
use sqlx::Row;
use sqlx::Sqlite;

use super::super::StateRuntime;
use super::namespace::ensure_agent_alias_namespace_in_transaction;
use super::namespace::insert_agent_alias_in_transaction;
use super::namespace::is_main_agent_nickname;
use super::namespace::release_matching_nickname_reservation_in_transaction;
use super::queries::find_agent_alias_by_thread_in_transaction;
use super::queries::find_current_agent_alias_by_thread_in_transaction;
use super::queries::find_spawn_parent_in_transaction;
use super::queries::require_active_parent_alias;
use crate::DirectionalThreadSpawnEdgeStatus;

struct TransferMember {
    thread_id: ThreadId,
    previous_parent_thread_id: Option<ThreadId>,
    state: crate::AgentAliasState,
    nickname: Option<String>,
    current_owner: Option<crate::AgentAliasRecord>,
}

impl StateRuntime {
    /// Exclusively transfer one target and its persisted subtree into another root.
    pub async fn transfer_agent_alias(
        &self,
        request: crate::AgentAliasTransferRequest,
    ) -> anyhow::Result<crate::AgentAliasTransfer> {
        let crate::AgentAliasTransferRequest {
            expected_previous_session_id,
            expected_descendant_thread_ids,
            new_session_id,
            new_parent_thread_id,
            thread_id,
            nickname,
            authored_selector,
        } = request;
        validate_transfer_request(
            new_session_id,
            new_parent_thread_id,
            thread_id,
            nickname.as_deref(),
            &authored_selector,
        )?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_agent_alias_namespace_in_transaction(&mut tx, new_session_id).await?;
        require_active_parent_alias(&mut tx, new_session_id, new_parent_thread_id).await?;

        let current_owner =
            find_current_agent_alias_by_thread_in_transaction(&mut tx, thread_id).await?;
        let actual_previous_session_id = current_owner.as_ref().map(|alias| alias.session_id);
        if actual_previous_session_id != expected_previous_session_id {
            anyhow::bail!(
                "agent {thread_id} ownership changed: expected {}, found {}",
                optional_session_id(expected_previous_session_id),
                optional_session_id(actual_previous_session_id),
            );
        }
        if let Some(current_owner) = current_owner.as_ref()
            && current_owner.session_id == new_session_id
        {
            tx.commit().await?;
            return Ok(crate::AgentAliasTransfer::AlreadyOwned {
                alias: current_owner.clone(),
            });
        }
        if let Some(previous_session_id) = actual_previous_session_id {
            // Complete any lazy migration before taking the subtree snapshot. Every persisted
            // descendant must then have the same exclusive owner as the selected target.
            ensure_agent_alias_namespace_in_transaction(&mut tx, previous_session_id).await?;
        }

        let previous_parent_thread_id =
            find_spawn_parent_in_transaction(&mut tx, thread_id).await?;
        let destination_alias =
            find_agent_alias_by_thread_in_transaction(&mut tx, new_session_id, thread_id).await?;
        if let (Some(destination_alias), Some(requested_nickname)) =
            (destination_alias.as_ref(), nickname.as_deref())
            && let Some(reserved_nickname) = destination_alias.nickname.as_deref()
            && requested_nickname != reserved_nickname
        {
            anyhow::bail!(
                "agent {thread_id} already reserves nickname {reserved_nickname:?} in session \
                 {new_session_id}; retry adoption using that durable identity"
            );
        }
        if let Some(requested_nickname) = nickname.as_deref()
            && available_transferred_nickname(
                &mut tx,
                new_session_id,
                thread_id,
                Some(requested_nickname),
            )
            .await?
            .is_none()
        {
            anyhow::bail!(
                "agent nickname {requested_nickname:?} is already reserved in session \
                 {new_session_id}; retry adoption to choose another nickname"
            );
        }
        let members = load_transfer_members(
            &mut tx,
            thread_id,
            previous_parent_thread_id,
            nickname,
            actual_previous_session_id,
        )
        .await?;
        let mut expected_descendant_thread_ids = expected_descendant_thread_ids;
        expected_descendant_thread_ids.sort_by_key(ToString::to_string);
        expected_descendant_thread_ids.dedup();
        let mut actual_descendant_thread_ids = members
            .iter()
            .skip(1)
            .map(|member| member.thread_id)
            .collect::<Vec<_>>();
        actual_descendant_thread_ids.sort_by_key(ToString::to_string);
        if actual_descendant_thread_ids != expected_descendant_thread_ids {
            anyhow::bail!(
                "agent {thread_id} subtree changed while rollout writers were being reserved"
            );
        }
        if members
            .iter()
            .skip(1)
            .any(|member| member.thread_id == new_parent_thread_id)
        {
            anyhow::bail!(
                "agent {thread_id} cannot be adopted beneath its own descendant {new_parent_thread_id}"
            );
        }

        tombstone_previous_aliases(&mut tx, &members).await?;
        let transferred_at_ms = Utc::now().timestamp_millis();
        let (target, descendants) = members
            .split_first()
            .context("transferred subtree omitted its selected target")?;
        let target_alias = activate_transferred_member(&mut tx, new_session_id, target).await?;
        record_transfer(
            &mut tx,
            target,
            actual_previous_session_id,
            new_session_id,
            new_parent_thread_id,
            &authored_selector,
            transferred_at_ms,
        )
        .await?;
        for member in descendants {
            activate_transferred_member(&mut tx, new_session_id, member).await?;
            record_transfer(
                &mut tx,
                member,
                actual_previous_session_id,
                new_session_id,
                member
                    .previous_parent_thread_id
                    .context("persisted descendant has no parent edge")?,
                &authored_selector,
                transferred_at_ms,
            )
            .await?;
        }

        super::super::threads::upsert_thread_spawn_edge_in_transaction(
            &mut tx,
            new_parent_thread_id,
            thread_id,
            DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await?;
        tx.commit().await?;

        Ok(crate::AgentAliasTransfer::Transferred {
            alias: target_alias,
            previous_session_id: actual_previous_session_id,
            previous_parent_thread_id,
            transferred_at_ms,
        })
    }
}

fn validate_transfer_request(
    new_session_id: SessionId,
    new_parent_thread_id: ThreadId,
    thread_id: ThreadId,
    nickname: Option<&str>,
    authored_selector: &str,
) -> anyhow::Result<()> {
    if thread_id == ThreadId::from(new_session_id) {
        anyhow::bail!("Main already owns ref 1 and cannot be adopted as its own child");
    }
    if thread_id == new_parent_thread_id {
        anyhow::bail!("an adopted agent cannot be its own parent");
    }
    if nickname.is_some_and(str::is_empty) {
        anyhow::bail!("an agent nickname cannot be empty");
    }
    if nickname.is_some_and(is_main_agent_nickname) {
        anyhow::bail!("Main is reserved for the destination root thread");
    }
    if authored_selector.is_empty() {
        anyhow::bail!("an ownership transfer requires the authored selector");
    }
    Ok(())
}

async fn load_transfer_members(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    thread_id: ThreadId,
    previous_parent_thread_id: Option<ThreadId>,
    requested_nickname: Option<String>,
    expected_previous_session_id: Option<SessionId>,
) -> anyhow::Result<Vec<TransferMember>> {
    let target_owner = find_current_agent_alias_by_thread_in_transaction(tx, thread_id).await?;
    validate_member_owner(
        thread_id,
        target_owner.as_ref(),
        expected_previous_session_id,
    )?;
    let target_nickname = requested_nickname.or_else(|| {
        target_owner.as_ref().and_then(|alias| {
            // A foreign root becomes an ordinary child at adoption. Its source namespace keeps
            // the reserved Main identity; production resume supplies a newly generated nickname
            // for the destination child.
            if alias.agent_ref == 1 {
                None
            } else {
                alias.nickname.clone()
            }
        })
    });
    let mut members = vec![TransferMember {
        thread_id,
        previous_parent_thread_id,
        state: crate::AgentAliasState::Active,
        nickname: target_nickname,
        current_owner: target_owner,
    }];

    let rows = sqlx::query(
        r#"
WITH RECURSIVE subtree(thread_id) AS (
    SELECT child_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id = ?
    UNION
    SELECT edge.child_thread_id
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.thread_id
)
SELECT
    subtree.thread_id,
    edge.parent_thread_id,
    edge.status,
    threads.agent_nickname
FROM subtree
JOIN thread_spawn_edges AS edge ON edge.child_thread_id = subtree.thread_id
LEFT JOIN threads ON threads.id = subtree.thread_id
ORDER BY subtree.thread_id
        "#,
    )
    .bind(thread_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    for row in rows {
        let descendant_thread_id = ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?;
        if descendant_thread_id == thread_id {
            anyhow::bail!("agent {thread_id} belongs to a cyclic persisted spawn graph");
        }
        let current_owner =
            find_current_agent_alias_by_thread_in_transaction(tx, descendant_thread_id).await?;
        validate_member_owner(
            descendant_thread_id,
            current_owner.as_ref(),
            expected_previous_session_id,
        )?;
        let edge_status = row
            .try_get::<String, _>("status")?
            .parse::<DirectionalThreadSpawnEdgeStatus>()?;
        let state = match edge_status {
            DirectionalThreadSpawnEdgeStatus::Open => crate::AgentAliasState::Active,
            DirectionalThreadSpawnEdgeStatus::Closed => crate::AgentAliasState::Closed,
        };
        members.push(TransferMember {
            thread_id: descendant_thread_id,
            previous_parent_thread_id: Some(ThreadId::try_from(
                row.try_get::<String, _>("parent_thread_id")?,
            )?),
            state,
            nickname: current_owner
                .as_ref()
                .and_then(|alias| alias.nickname.clone())
                .or(row.try_get::<Option<String>, _>("agent_nickname")?),
            current_owner,
        });
    }
    Ok(members)
}

fn validate_member_owner(
    thread_id: ThreadId,
    current_owner: Option<&crate::AgentAliasRecord>,
    expected_previous_session_id: Option<SessionId>,
) -> anyhow::Result<()> {
    let actual_session_id = current_owner.map(|alias| alias.session_id);
    if actual_session_id != expected_previous_session_id {
        anyhow::bail!(
            "agent subtree has mixed ownership at {thread_id}: expected {}, found {}",
            optional_session_id(expected_previous_session_id),
            optional_session_id(actual_session_id),
        );
    }
    Ok(())
}

async fn tombstone_previous_aliases(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    members: &[TransferMember],
) -> anyhow::Result<()> {
    for member in members {
        let Some(current_owner) = member.current_owner.as_ref() else {
            continue;
        };
        let result = sqlx::query(
            r#"
UPDATE agent_aliases
SET ownership_state = ?
WHERE session_id = ?
  AND thread_id = ?
  AND ownership_state = ?
            "#,
        )
        .bind(super::AgentAliasOwnershipState::Transferred.as_ref())
        .bind(current_owner.session_id.to_string())
        .bind(member.thread_id.to_string())
        .bind(super::AgentAliasOwnershipState::Current.as_ref())
        .execute(&mut **tx)
        .await?;
        if result.rows_affected() != 1 {
            anyhow::bail!(
                "agent {} no longer has the expected current owner",
                member.thread_id
            );
        }
    }
    Ok(())
}

async fn activate_transferred_member(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    new_session_id: SessionId,
    member: &TransferMember,
) -> anyhow::Result<crate::AgentAliasRecord> {
    let existing =
        find_agent_alias_by_thread_in_transaction(tx, new_session_id, member.thread_id).await?;
    let Some(existing) = existing else {
        let nickname = available_transferred_nickname(
            tx,
            new_session_id,
            member.thread_id,
            member.nickname.as_deref(),
        )
        .await?;
        return insert_agent_alias_in_transaction(
            tx,
            new_session_id,
            member.thread_id,
            nickname.as_deref(),
            member.state,
        )
        .await;
    };
    if existing.state != crate::AgentAliasState::Transferred {
        anyhow::bail!(
            "agent {} already has a current alias in session {new_session_id}",
            member.thread_id
        );
    }

    let nickname = if existing.nickname.is_some() {
        existing.nickname.clone()
    } else {
        available_transferred_nickname(
            tx,
            new_session_id,
            member.thread_id,
            member.nickname.as_deref(),
        )
        .await?
    };
    release_matching_nickname_reservation_in_transaction(
        tx,
        new_session_id,
        member.thread_id,
        nickname.as_deref(),
    )
    .await?;
    let result = sqlx::query(
        r#"
UPDATE agent_aliases
SET ownership_state = ?, nickname = ?
WHERE session_id = ?
  AND thread_id = ?
  AND ownership_state = ?
        "#,
    )
    .bind(super::AgentAliasOwnershipState::Current.as_ref())
    .bind(&nickname)
    .bind(new_session_id.to_string())
    .bind(member.thread_id.to_string())
    .bind(super::AgentAliasOwnershipState::Transferred.as_ref())
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() != 1 {
        anyhow::bail!(
            "agent {} no longer has its reserved alias in session {new_session_id}",
            member.thread_id
        );
    }
    Ok(crate::AgentAliasRecord {
        session_id: new_session_id,
        thread_id: member.thread_id,
        agent_ref: existing.agent_ref,
        nickname,
        state: member.state,
    })
}

async fn available_transferred_nickname(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    session_id: SessionId,
    thread_id: ThreadId,
    nickname: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(nickname) = nickname else {
        return Ok(None);
    };
    if is_main_agent_nickname(nickname) {
        return Ok(None);
    }
    let thread_id = thread_id.to_string();
    let alias_owner = sqlx::query_scalar::<_, String>(
        "SELECT thread_id FROM agent_aliases WHERE session_id = ? AND nickname = ?",
    )
    .bind(session_id.to_string())
    .bind(nickname)
    .fetch_optional(&mut **tx)
    .await?;
    if alias_owner
        .as_deref()
        .is_some_and(|owner| owner != thread_id.as_str())
    {
        return Ok(None);
    }
    let reservation_owner = sqlx::query_scalar::<_, String>(
        "SELECT source_thread_id FROM agent_alias_nickname_reservations WHERE session_id = ? AND nickname = ?",
    )
    .bind(session_id.to_string())
    .bind(nickname)
    .fetch_optional(&mut **tx)
    .await?;
    if reservation_owner
        .as_deref()
        .is_some_and(|owner| owner != thread_id.as_str())
    {
        return Ok(None);
    }
    Ok(Some(nickname.to_string()))
}

async fn record_transfer(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    member: &TransferMember,
    previous_session_id: Option<SessionId>,
    new_session_id: SessionId,
    new_parent_thread_id: ThreadId,
    authored_selector: &str,
    transferred_at_ms: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
INSERT INTO agent_alias_transfers (
    thread_id,
    previous_session_id,
    new_session_id,
    previous_parent_thread_id,
    new_parent_thread_id,
    authored_selector,
    transferred_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(member.thread_id.to_string())
    .bind(previous_session_id.map(|session_id| session_id.to_string()))
    .bind(new_session_id.to_string())
    .bind(
        member
            .previous_parent_thread_id
            .map(|thread_id| thread_id.to_string()),
    )
    .bind(new_parent_thread_id.to_string())
    .bind(authored_selector)
    .bind(transferred_at_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn optional_session_id(session_id: Option<SessionId>) -> String {
    session_id.map_or_else(|| "none".to_string(), |session_id| session_id.to_string())
}
