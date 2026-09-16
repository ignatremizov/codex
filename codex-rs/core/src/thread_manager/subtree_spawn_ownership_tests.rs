use super::*;
use crate::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use futures::FutureExt;
use pretty_assertions::assert_eq;

enum SpawnResolution {
    Publish,
    Cancel,
    FailRollback,
    FailAliasRollback,
    FailShutdownAndAliasRollback,
}

async fn spawn_racing_sibling_unload(resolution: SpawnResolution) {
    let home = tempdir().expect("home");
    let mut config = test_config().await;
    config.codex_home = home.path().abs();
    config.cwd = config.codex_home.abs();
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let state_db = init_state_db(&config).await;
    let graph = Arc::new(GatedTransferAgentGraphStore::with_allocation_gate(
        local_agent_graph_store_from_state_db(state_db.as_ref()).expect("alias store"),
    ));
    let store = Arc::new(InMemoryThreadStore::default());
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test"));
    let manager = Arc::new(ThreadManager::new(
        &config,
        Arc::clone(&auth),
        build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        store.clone(),
        Some(graph.clone()),
        TEST_INSTALLATION_ID.to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    ));
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .expect("root");
    graph.release_allocation.notify_one();
    let sibling = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("sibling");
    graph.allocation_started.notified().await;

    let spawning_root = Arc::clone(&root.thread);
    let spawn = tokio::spawn(async move {
        spawning_root
            .spawn_agent(UserAgentSpawnOptions::default())
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        graph.allocation_started.notified(),
    )
    .await
    .expect("idle spawn reaches alias allocation");
    let pending = manager
        .state
        .pending_threads
        .read()
        .await
        .values()
        .filter_map(std::sync::Weak::upgrade)
        .collect::<Vec<_>>();
    assert_eq!(pending.len(), 1);
    let child = Arc::clone(&pending[0]);
    drop(pending);
    let child_id = child.session.thread_id();
    let mut unload = Box::pin(manager.prepare_subtree_unload(sibling.target_thread_id));
    assert!(unload.as_mut().now_or_never().is_none());
    if !matches!(resolution, SpawnResolution::Publish) {
        spawn.abort();
        assert!(spawn.await.expect_err("caller cancelled").is_cancelled());
        // The detached rollback still owns the parent's membership fence.
        assert!(unload.as_mut().now_or_never().is_none());
        if matches!(
            resolution,
            SpawnResolution::FailRollback | SpawnResolution::FailShutdownAndAliasRollback
        ) {
            store
                .fail_next_operation(InMemoryThreadStoreFailure::ThreadShutdown)
                .await;
        }
        if matches!(
            resolution,
            SpawnResolution::FailAliasRollback | SpawnResolution::FailShutdownAndAliasRollback
        ) {
            graph.fail_next_close.store(/*val*/ true, Ordering::Release);
        }
        graph.release_allocation.notify_one();
    } else {
        graph.release_allocation.notify_one();
        assert_eq!(
            spawn
                .await
                .expect("spawn task")
                .expect("published")
                .target_thread_id,
            child_id
        );
    }
    let mut subtree = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), unload)
        .await
        .expect("membership fence released")
        .expect("capture subtree");
    assert_eq!(subtree.root_thread_id(), root.thread_id);
    match resolution {
        SpawnResolution::Publish => assert!(subtree.thread_ids().contains(&child_id)),
        SpawnResolution::Cancel => {
            assert!(!subtree.thread_ids().contains(&child_id));
            assert!(child.io.durable_shutdown_succeeded());
        }
        SpawnResolution::FailRollback
        | SpawnResolution::FailAliasRollback
        | SpawnResolution::FailShutdownAndAliasRollback => {
            assert!(subtree.thread_ids().contains(&child_id));
            assert!(child.ensure_not_unloading().is_err());
            assert!(Arc::ptr_eq(
                &manager.get_thread(child_id).await.expect("retained child"),
                &child
            ));
            assert_eq!(
                child
                    .cancelled_spawn_alias_cleanup_pending
                    .load(Ordering::Acquire),
                matches!(
                    resolution,
                    SpawnResolution::FailAliasRollback
                        | SpawnResolution::FailShutdownAndAliasRollback
                ),
            );
            if matches!(resolution, SpawnResolution::FailAliasRollback) {
                assert!(child.io.durable_shutdown_succeeded());
            }
        }
    }
    subtree
        .shutdown_and_remove()
        .await
        .expect("unload finishes or retries rollback");
    assert!(manager.list_thread_ids().await.is_empty());
    let alias = graph
        .find_current_agent_alias_by_thread(child_id)
        .await
        .expect("alias lookup")
        .expect("durable alias");
    assert_eq!(
        alias.state,
        match resolution {
            SpawnResolution::Publish => codex_agent_graph_store::AgentAliasState::Active,
            SpawnResolution::Cancel
            | SpawnResolution::FailRollback
            | SpawnResolution::FailAliasRollback
            | SpawnResolution::FailShutdownAndAliasRollback =>
                codex_agent_graph_store::AgentAliasState::Closed,
        }
    );
}

#[tokio::test]
async fn public_idle_spawn_is_captured_by_sibling_unload() {
    spawn_racing_sibling_unload(SpawnResolution::Publish).await;
}

#[tokio::test]
async fn cancelled_idle_spawn_keeps_parent_fenced_until_rollback() {
    spawn_racing_sibling_unload(SpawnResolution::Cancel).await;
}

#[tokio::test]
async fn failed_cancelled_spawn_rollback_is_retained_for_unload_retry() {
    spawn_racing_sibling_unload(SpawnResolution::FailRollback).await;
}

#[tokio::test]
async fn cancelled_spawn_alias_failure_retains_stopped_runtime_until_retry() {
    spawn_racing_sibling_unload(SpawnResolution::FailAliasRollback).await;
}

#[tokio::test]
async fn cancelled_spawn_shutdown_and_alias_failures_both_complete_on_retry() {
    spawn_racing_sibling_unload(SpawnResolution::FailShutdownAndAliasRollback).await;
}
