use crate::AgentAliasAllocation;
use crate::AgentAliasDetachRequest;
use crate::AgentAliasState;
use crate::AgentAliasTransferRequest;
use crate::DirectionalThreadSpawnEdgeStatus;
use crate::SqliteConfig;
use crate::StateRuntime;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;

async fn fixture() -> anyhow::Result<(
    scopeguard::ScopeGuard<PathBuf, fn(PathBuf)>,
    Arc<StateRuntime>,
    [ThreadId; 4],
)> {
    let cleanup: fn(PathBuf) = |home| {
        let _ = std::fs::remove_dir_all(home);
    };
    let home = scopeguard::guard(unique_temp_dir(), cleanup);
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".into(),
    )
    .await?;
    let ids = std::array::from_fn(|_| ThreadId::new());
    let [root, child, descendant, sibling] = ids;
    for (parent, id, nickname, task) in [
        (root, child, "Child", "/root/work"),
        (child, descendant, "Reviewer", "/root/work/check"),
        (root, sibling, "Sibling", "/root/other"),
    ] {
        runtime
            .allocate_agent_alias(AgentAliasAllocation {
                session_id: root.into(),
                parent_thread_id: parent,
                child_thread_id: id,
                nickname: Some(nickname.into()),
                task_path: Some(task.into()),
            })
            .await?;
    }
    runtime
        .set_agent_lifecycle_state(
            root.into(),
            descendant,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await?;
    Ok((home, runtime, ids))
}

#[tokio::test]
async fn promotion_preserves_ids_closed_descendants_and_original_alias_reservations()
-> anyhow::Result<()> {
    let (_home, runtime, [root, child, descendant, sibling]) = fixture().await?;
    let original = runtime.list_agent_aliases(root.into()).await?;
    runtime
        .detach_agent_alias(AgentAliasDetachRequest {
            thread_id: child,
            expected_previous_session_id: root.into(),
            expected_descendant_thread_ids: vec![descendant],
        })
        .await?;
    let mut expected_original = original;
    for alias in &mut expected_original {
        if [child, descendant].contains(&alias.thread_id) {
            alias.state = AgentAliasState::Transferred;
        }
    }
    assert_eq!(
        runtime.list_agent_aliases(root.into()).await?,
        expected_original
    );
    let promoted = runtime.list_agent_aliases(child.into()).await?;
    assert_eq!(
        promoted
            .iter()
            .map(|alias| (
                alias.thread_id,
                alias.agent_ref,
                alias.nickname.as_deref(),
                alias.task_path.as_deref(),
                alias.state,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                child,
                1,
                Some("Main"),
                Some("/root"),
                AgentAliasState::Active
            ),
            (
                descendant,
                2,
                Some("Reviewer"),
                Some("/root/check"),
                AgentAliasState::Closed
            ),
        ],
    );
    assert_eq!(
        (
            runtime.find_thread_spawn_parent(child).await?,
            runtime.find_thread_spawn_parent(descendant).await?,
            runtime.find_thread_spawn_parent(sibling).await?,
        ),
        (None, Some(child), Some(root)),
    );
    assert_eq!(
        runtime
            .read_agent_thread_lifecycle_epochs(&[child, descendant])
            .await?,
        vec![(child, 1), (descendant, 2)],
    );
    Ok(())
}

#[tokio::test]
async fn failed_detach_rolls_back_owner_topology_labels_epochs_and_audit() -> anyhow::Result<()> {
    let (_home, runtime, [root, child, descendant, _]) = fixture().await?;
    let aliases = runtime.list_agent_aliases(root.into()).await?;
    let epochs = runtime
        .read_agent_thread_lifecycle_epochs(&[child, descendant])
        .await?;
    let request = AgentAliasDetachRequest {
        thread_id: child,
        expected_previous_session_id: root.into(),
        expected_descendant_thread_ids: vec![descendant],
    };
    assert!(
        runtime
            .detach_agent_alias(AgentAliasDetachRequest {
                expected_previous_session_id: SessionId::from(ThreadId::new()),
                ..request.clone()
            })
            .await
            .is_err()
    );
    assert!(
        runtime
            .detach_agent_alias(AgentAliasDetachRequest {
                expected_descendant_thread_ids: Vec::new(),
                ..request.clone()
            })
            .await
            .is_err()
    );
    sqlx::query(
        "CREATE TRIGGER fail_detach BEFORE INSERT ON agent_root_detachments \
         BEGIN SELECT RAISE(ABORT, 'injected detach failure'); END",
    )
    .execute(runtime.pool.as_ref())
    .await?;
    assert!(runtime.detach_agent_alias(request).await.is_err());
    assert_eq!(
        (
            runtime.list_agent_aliases(root.into()).await?,
            runtime.find_thread_spawn_parent(child).await?,
            runtime
                .read_agent_thread_lifecycle_epochs(&[child, descendant])
                .await?,
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_root_detachments")
                .fetch_one(runtime.pool.as_ref())
                .await?,
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM agent_alias_namespaces WHERE session_id = ?"
            )
            .bind(child.to_string())
            .fetch_one(runtime.pool.as_ref())
            .await?,
        ),
        (aliases, Some(root), epochs, 0, 0),
    );
    Ok(())
}

#[tokio::test]
async fn reattachment_and_second_detach_reuse_each_roots_reserved_refs() -> anyhow::Result<()> {
    let (_home, runtime, [root, child, descendant, _]) = fixture().await?;
    let request = AgentAliasDetachRequest {
        thread_id: child,
        expected_previous_session_id: root.into(),
        expected_descendant_thread_ids: vec![descendant],
    };
    runtime.detach_agent_alias(request.clone()).await?;
    let promoted = runtime.list_agent_aliases(child.into()).await?;
    runtime
        .transfer_agent_alias(AgentAliasTransferRequest {
            expected_previous_session_id: Some(child.into()),
            expected_descendant_thread_ids: vec![descendant],
            new_session_id: root.into(),
            new_parent_thread_id: root,
            thread_id: child,
            nickname: None,
            task_path: Some("/root/work".into()),
            authored_selector: child.to_string(),
        })
        .await?;
    let reattached = runtime.list_agent_aliases(root.into()).await?;
    assert_eq!(
        reattached
            .iter()
            .filter(|alias| [child, descendant].contains(&alias.thread_id))
            .map(|alias| (alias.thread_id, alias.agent_ref, alias.state))
            .collect::<Vec<_>>(),
        vec![
            (child, 2, AgentAliasState::Active),
            (descendant, 3, AgentAliasState::Closed)
        ],
    );
    runtime.detach_agent_alias(request).await?;
    assert_eq!(runtime.list_agent_aliases(child.into()).await?, promoted);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_root_detachments")
            .fetch_one(runtime.pool.as_ref())
            .await?,
        2,
    );
    Ok(())
}

#[tokio::test]
async fn promotion_resolves_assignment_collisions_without_changing_thread_identity()
-> anyhow::Result<()> {
    let (_home, runtime, [root, child, descendant, _]) = fixture().await?;
    let other = ThreadId::new();
    runtime
        .allocate_agent_alias(AgentAliasAllocation {
            session_id: root.into(),
            parent_thread_id: child,
            child_thread_id: other,
            nickname: Some("Other".into()),
            task_path: Some("/root/check".into()),
        })
        .await?;
    runtime
        .detach_agent_alias(AgentAliasDetachRequest {
            thread_id: child,
            expected_previous_session_id: root.into(),
            expected_descendant_thread_ids: vec![descendant, other],
        })
        .await?;
    let mut labels = runtime
        .list_agent_aliases(child.into())
        .await?
        .into_iter()
        .filter(|alias| alias.thread_id != child)
        .map(|alias| alias.task_path)
        .collect::<Vec<_>>();
    labels.sort();
    assert_eq!(
        labels,
        vec![Some("/root/check".into()), Some("/root/check-2".into())]
    );
    assert_eq!(runtime.find_thread_spawn_parent(other).await?, Some(child));
    Ok(())
}

#[tokio::test]
async fn historical_metadata_refresh_preserves_the_promoted_root_boundary() -> anyhow::Result<()> {
    let (home, runtime, [root, child, descendant, _]) = fixture().await?;
    let mut metadata = crate::runtime::test_support::test_thread_metadata(
        home.as_path(),
        child,
        home.as_path().to_path_buf(),
    );
    metadata.source =
        serde_json::to_string(&SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root,
            depth: 1,
            agent_path: None,
            agent_nickname: Some("Child".into()),
            agent_role: None,
        }))?;
    runtime.upsert_thread(&metadata).await?;
    runtime
        .detach_agent_alias(AgentAliasDetachRequest {
            thread_id: child,
            expected_previous_session_id: root.into(),
            expected_descendant_thread_ids: vec![descendant],
        })
        .await?;
    // Both indexing paths can replay the historical source after a cold restart.
    runtime.upsert_thread(&metadata).await?;
    runtime.insert_thread_if_absent(&metadata).await?;
    assert_eq!(
        (
            runtime.find_thread_spawn_parent(child).await?,
            runtime.find_thread_spawn_parent(descendant).await?,
            runtime
                .find_current_agent_alias_by_thread(child)
                .await?
                .map(|alias| (alias.session_id, alias.agent_ref)),
        ),
        (None, Some(child), Some((child.into(), 1))),
    );
    Ok(())
}
