use super::*;
use crate::UserAgentSpawnOptions;
use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentGraphStore;
use codex_agent_graph_store::AgentGraphStoreError;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_agent_graph_store::AgentLifecycleAuthorityUpdate;
use codex_agent_graph_store::AgentSendScope;
use codex_agent_graph_store::AgentSendSetting;
use codex_agent_graph_store::AllocateAgentAliasRequest;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_features::Feature;
use codex_protocol::SessionId;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;
use tokio::sync::Notify;

/// Holds real alias allocation while the deferred runtime still owns its parent fence.
struct GatedAllocationAgentGraphStore {
    inner: Arc<dyn AgentGraphStore>,
    allocation_started: Notify,
    release_allocation: Notify,
    allocating_thread: Mutex<Option<ThreadId>>,
    fail_next_close: AtomicBool,
}

impl AgentGraphStore for GatedAllocationAgentGraphStore {
    fn supports_agent_aliases(&self) -> bool {
        self.inner.supports_agent_aliases()
    }

    fn supports_agent_send_settings(&self) -> bool {
        self.inner.supports_agent_send_settings()
    }

    fn read_agent_send_settings(
        &self,
        scopes: Vec<AgentSendScope>,
    ) -> AgentGraphStoreFuture<'_, Vec<AgentSendSetting>> {
        self.inner.read_agent_send_settings(scopes)
    }

    fn read_thread_lifecycle_authority_epochs(
        &self,
        thread_ids: Vec<ThreadId>,
    ) -> AgentGraphStoreFuture<'_, Vec<(ThreadId, i64)>> {
        self.inner
            .read_thread_lifecycle_authority_epochs(thread_ids)
    }

    fn ensure_agent_alias_namespace(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        self.inner.ensure_agent_alias_namespace(session_id)
    }

    fn allocate_agent_alias(
        &self,
        request: AllocateAgentAliasRequest,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        Box::pin(async move {
            *self.allocating_thread.lock().await = Some(request.child_thread_id);
            self.allocation_started.notify_one();
            self.release_allocation.notified().await;
            self.inner.allocate_agent_alias(request).await
        })
    }

    fn set_agent_lifecycle_state_with_authority_revocations(
        &self,
        session_id: SessionId,
        thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
        revoked_thread_ids: Vec<ThreadId>,
    ) -> AgentGraphStoreFuture<'_, AgentLifecycleAuthorityUpdate> {
        Box::pin(async move {
            if status == ThreadSpawnEdgeStatus::Closed
                && self.fail_next_close.swap(/*val*/ false, Ordering::AcqRel)
            {
                return Err(AgentGraphStoreError::Internal {
                    message: "injected cancelled-spawn alias close failure".to_string(),
                });
            }
            self.inner
                .set_agent_lifecycle_state_with_authority_revocations(
                    session_id,
                    thread_id,
                    status,
                    revoked_thread_ids,
                )
                .await
        })
    }

    fn find_agent_alias_by_thread(
        &self,
        session_id: SessionId,
        thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        self.inner.find_agent_alias_by_thread(session_id, thread_id)
    }

    fn find_current_agent_alias_by_thread(
        &self,
        thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        self.inner.find_current_agent_alias_by_thread(thread_id)
    }

    fn list_agent_aliases(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, Vec<AgentAlias>> {
        self.inner.list_agent_aliases(session_id)
    }

    fn list_agent_nickname_reservations(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, Vec<String>> {
        self.inner.list_agent_nickname_reservations(session_id)
    }

    fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        self.inner
            .upsert_thread_spawn_edge(parent_thread_id, child_thread_id, status)
    }

    fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        self.inner
            .set_thread_spawn_edge_status(child_thread_id, status)
    }

    fn find_thread_spawn_parent(
        &self,
        child_thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<ThreadId>> {
        self.inner.find_thread_spawn_parent(child_thread_id)
    }

    fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.inner
            .list_thread_spawn_children(parent_thread_id, status_filter)
    }

    fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.inner
            .list_thread_spawn_descendants(root_thread_id, status_filter)
    }
}

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
    let graph = Arc::new(GatedAllocationAgentGraphStore {
        inner: local_agent_graph_store_from_state_db(state_db.as_ref()).expect("alias store"),
        allocation_started: Notify::new(),
        release_allocation: Notify::new(),
        allocating_thread: Mutex::new(None),
        fail_next_close: AtomicBool::new(/*val*/ false),
    });
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
        passthrough_image_store(),
        store.clone(),
        Some(graph.clone()),
        TEST_INSTALLATION_ID.to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    ));
    let root = manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config)
        })
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
    let mut spawn = tokio::spawn(async move {
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
    // Deferred runtimes are owned by SpawnCleanup, not a manager-side pending map.
    // Capture the actual allocation's ID and prove both lifecycle gates remain owned.
    let child_id = graph
        .allocating_thread
        .lock()
        .await
        .expect("allocating child");
    assert!(manager.get_thread(child_id).await.is_err());
    assert!(
        manager
            .state
            .agent_lifecycle_lock(child_id)
            .try_lock_owned()
            .is_err()
    );
    assert!(
        manager
            .state
            .agent_lifecycle_lock(root.thread_id)
            .try_lock_owned()
            .is_err()
    );
    let shutdowns_before = store.calls().await.shutdown_thread;
    let mut unload = Box::pin(manager.prepare_subtree_unload(sibling.target_thread_id));
    assert!(unload.as_mut().now_or_never().is_none());
    if !matches!(resolution, SpawnResolution::Publish) {
        spawn.abort();
        assert!(
            (&mut spawn)
                .await
                .expect_err("caller cancelled")
                .is_cancelled()
        );
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
    }
    let mut subtree = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        if matches!(resolution, SpawnResolution::Publish) {
            graph.release_allocation.notify_one();
            // Keep polling unload while publication finishes: it may already be queued on
            // a fair lifecycle mutex that the spawn's completion path needs to reacquire.
            let (spawned, captured) = tokio::join!(&mut spawn, unload);
            assert_eq!(
                spawned
                    .expect("spawn task")
                    .expect("published")
                    .target_thread_id,
                child_id
            );
            captured
        } else {
            unload.await
        }
    })
    .await
    .expect("membership fence released")
    .expect("capture subtree");
    assert_eq!(subtree.root_thread_id(), root.thread_id);
    let retained_child = manager.get_thread(child_id).await.ok();
    match resolution {
        SpawnResolution::Publish => assert!(subtree.thread_ids().contains(&child_id)),
        SpawnResolution::Cancel => {
            assert!(!subtree.thread_ids().contains(&child_id));
            assert!(manager.get_thread(child_id).await.is_err());
            assert_eq!(store.calls().await.shutdown_thread, shutdowns_before + 1);
        }
        SpawnResolution::FailRollback
        | SpawnResolution::FailAliasRollback
        | SpawnResolution::FailShutdownAndAliasRollback => {
            assert!(subtree.thread_ids().contains(&child_id));
            let child = retained_child.as_ref().expect("retained child");
            assert!(child.ensure_not_unloading().is_err());
            assert!(Arc::ptr_eq(
                &manager.get_thread(child_id).await.expect("retained child"),
                child
            ));
            assert_eq!(
                child.io.durable_shutdown_succeeded(),
                matches!(resolution, SpawnResolution::FailAliasRollback),
            );
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
        }
    }
    subtree
        .shutdown_and_remove()
        .await
        .expect("unload finishes or retries rollback");
    assert!(manager.list_thread_ids().await.is_empty());
    if let Some(child) = retained_child {
        // Retry must drain the captured runtime, not merely remove its manager entry.
        assert!(child.io.durable_shutdown_succeeded());
        assert!(
            !child
                .cancelled_spawn_alias_cleanup_pending
                .load(Ordering::Acquire)
        );
    }
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
