//! Late legacy discovery fills identity gaps without reopening graph edges.

use super::*;
use crate::AgentGraphStoreResult;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn late_alias_backfill_preserves_lifecycle_existing_labels_and_stable_refs()
-> AgentGraphStoreResult<()> {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root = thread_id(/*suffix*/ 1800);
    let known = thread_id(/*suffix*/ 1801);
    let closed = thread_id(/*suffix*/ 1802);
    let descendant = thread_id(/*suffix*/ 1803);
    let session_id = SessionId::from(root);
    let root_alias = store.ensure_agent_alias_namespace(session_id).await?;
    let known_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root,
            child_thread_id: known,
            nickname: Some("Kepler".into()),
            task_path: Some("/root/known".into()),
        })
        .await?;

    // Index topology after the namespace's one-time migration scan.
    store
        .upsert_thread_spawn_edge(root, closed, ThreadSpawnEdgeStatus::Closed)
        .await?;
    store
        .upsert_thread_spawn_edge(closed, descendant, ThreadSpawnEdgeStatus::Open)
        .await?;
    assert_eq!(
        store.find_current_agent_alias_by_thread(closed).await?,
        None
    );

    let expected = vec![
        root_alias,
        known_alias,
        AgentAlias {
            session_id,
            thread_id: closed,
            agent_ref: 3,
            nickname: None,
            task_path: None,
            state: AgentAliasState::Closed,
        },
        AgentAlias {
            session_id,
            thread_id: descendant,
            agent_ref: 4,
            nickname: None,
            task_path: None,
            state: AgentAliasState::Active,
        },
    ];
    for _ in 0..2 {
        store.backfill_agent_aliases(session_id).await?;
        assert_eq!(store.list_agent_aliases(session_id).await?, expected);
        assert_eq!(
            store
                .list_thread_spawn_children(root, Some(ThreadSpawnEdgeStatus::Closed))
                .await?,
            vec![closed]
        );
        assert_eq!(
            store.find_thread_spawn_parent(descendant).await?,
            Some(closed)
        );
    }
    let next_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root,
            child_thread_id: thread_id(/*suffix*/ 1804),
            nickname: None,
            task_path: None,
        })
        .await?;
    assert_eq!(
        next_alias,
        AgentAlias {
            session_id,
            thread_id: thread_id(/*suffix*/ 1804),
            agent_ref: 5,
            nickname: None,
            task_path: None,
            state: AgentAliasState::Active,
        }
    );
    Ok(())
}

#[tokio::test]
async fn late_alias_backfill_rolls_back_when_discovered_ancestry_is_cyclic()
-> AgentGraphStoreResult<()> {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root = thread_id(/*suffix*/ 1810);
    let child = thread_id(/*suffix*/ 1811);
    let session_id = SessionId::from(root);
    let root_alias = store.ensure_agent_alias_namespace(session_id).await?;
    store
        .upsert_thread_spawn_edge(root, child, ThreadSpawnEdgeStatus::Open)
        .await?;
    store
        .upsert_thread_spawn_edge(child, root, ThreadSpawnEdgeStatus::Open)
        .await?;

    assert!(store.backfill_agent_aliases(session_id).await.is_err());
    assert_eq!(
        store.list_agent_aliases(session_id).await?,
        vec![root_alias]
    );
    Ok(())
}

#[tokio::test]
async fn late_alias_backfill_cannot_replace_an_existing_foreign_owner() -> AgentGraphStoreResult<()>
{
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root = thread_id(/*suffix*/ 1820);
    let foreign_root = thread_id(/*suffix*/ 1821);
    let child = thread_id(/*suffix*/ 1822);
    let session_id = SessionId::from(root);
    let foreign_session_id = SessionId::from(foreign_root);
    let root_alias = store.ensure_agent_alias_namespace(session_id).await?;
    store
        .ensure_agent_alias_namespace(foreign_session_id)
        .await?;
    let foreign_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: foreign_session_id,
            parent_thread_id: foreign_root,
            child_thread_id: child,
            nickname: Some("Hopper".into()),
            task_path: Some("/root/owned".into()),
        })
        .await?;
    // Stale topology alone cannot authorize an ownership transfer.
    store
        .upsert_thread_spawn_edge(root, child, ThreadSpawnEdgeStatus::Open)
        .await?;

    assert!(store.backfill_agent_aliases(session_id).await.is_err());
    assert_eq!(
        store.list_agent_aliases(session_id).await?,
        vec![root_alias]
    );
    assert_eq!(
        store.find_current_agent_alias_by_thread(child).await?,
        Some(foreign_alias)
    );
    Ok(())
}
