use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::StateRuntime;
use crate::AgentAliasAllocation;
use crate::AgentAliasState;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;

#[tokio::test]
async fn preserving_deletion_retains_related_aliases_and_both_incident_edges() -> anyhow::Result<()>
{
    let home = unique_temp_dir();
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let root = ThreadId::new();
    let selected = ThreadId::new();
    let child = ThreadId::new();
    for thread_id in [root, selected, child] {
        runtime
            .upsert_thread(&test_thread_metadata(
                home.as_path(),
                thread_id,
                home.clone(),
            ))
            .await?;
    }
    let root_alias = runtime.ensure_agent_alias_namespace(root.into()).await?;
    let selected_request = AgentAliasAllocation {
        session_id: root.into(),
        parent_thread_id: root,
        child_thread_id: selected,
        nickname: Some("selected".to_string()),
    };
    let mut selected_alias = runtime
        .allocate_agent_alias(selected_request.clone())
        .await?;
    let child_alias = runtime
        .allocate_agent_alias(AgentAliasAllocation {
            session_id: root.into(),
            parent_thread_id: selected,
            child_thread_id: child,
            nickname: Some("child".to_string()),
        })
        .await?;
    let child_metadata = runtime.get_thread(child).await?;

    assert_eq!(
        runtime
            .delete_thread_preserving_agent_graph(selected)
            .await?,
        1
    );
    assert_eq!(runtime.get_thread(selected).await?, None);
    assert_eq!(runtime.get_thread(child).await?, child_metadata);
    assert_eq!(
        runtime.find_thread_spawn_parent(selected).await?,
        Some(root)
    );
    assert_eq!(
        runtime.find_thread_spawn_parent(child).await?,
        Some(selected)
    );
    assert_eq!(
        runtime.find_current_agent_alias_by_thread(selected).await?,
        None
    );
    assert_eq!(
        runtime.find_current_agent_alias_by_thread(child).await?,
        Some(child_alias.clone())
    );
    selected_alias.state = AgentAliasState::Closed;
    assert_eq!(
        runtime.list_agent_aliases(root.into()).await?,
        vec![root_alias, selected_alias, child_alias]
    );
    assert!(
        runtime
            .activate_agent_alias(selected_request)
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .delete_thread_preserving_agent_graph(selected)
            .await?,
        0
    );
    assert_eq!(
        runtime.find_thread_spawn_parent(child).await?,
        Some(selected)
    );

    // Explicit strict deletion retains its incident-edge removal semantics.
    assert_eq!(runtime.delete_threads_strict(&[selected]).await?, 0);
    assert_eq!(runtime.find_thread_spawn_parent(selected).await?, None);
    assert_eq!(runtime.find_thread_spawn_parent(child).await?, None);
    assert_eq!(runtime.get_thread(child).await?, child_metadata);
    Ok(())
}

#[tokio::test]
async fn preserving_delete_rolls_back_tombstone_with_final_transaction() -> anyhow::Result<()> {
    let home = unique_temp_dir();
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let root = ThreadId::new();
    let child = ThreadId::new();
    runtime
        .upsert_thread(&test_thread_metadata(home.as_path(), root, home.clone()))
        .await?;
    let metadata = runtime.get_thread(root).await?;
    let root_alias = runtime.ensure_agent_alias_namespace(root.into()).await?;
    runtime
        .allocate_agent_alias(AgentAliasAllocation {
            session_id: root.into(),
            parent_thread_id: root,
            child_thread_id: child,
            nickname: None,
        })
        .await?;
    sqlx::query(
        "CREATE TRIGGER fail_preserving_delete BEFORE DELETE ON threads BEGIN SELECT RAISE(ABORT, 'injected deletion failure'); END",
    )
    .execute(runtime.pool.as_ref())
    .await?;

    assert!(
        runtime
            .delete_thread_preserving_agent_graph(root)
            .await
            .is_err()
    );
    assert_eq!(runtime.get_thread(root).await?, metadata);
    assert_eq!(
        runtime.find_current_agent_alias_by_thread(root).await?,
        Some(root_alias)
    );
    assert_eq!(runtime.find_thread_spawn_parent(child).await?, Some(root));
    let tombstones: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_alias_tombstones")
        .fetch_one(runtime.pool.as_ref())
        .await?;
    assert_eq!(tombstones, 0);

    sqlx::query("DROP TRIGGER fail_preserving_delete")
        .execute(runtime.pool.as_ref())
        .await?;
    assert_eq!(runtime.delete_thread_preserving_agent_graph(root).await?, 1);
    assert_eq!(
        runtime.find_current_agent_alias_by_thread(root).await?,
        None
    );
    assert_eq!(runtime.find_thread_spawn_parent(child).await?, Some(root));
    Ok(())
}
