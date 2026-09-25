use super::*;
use crate::thread_manager::NewThread;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::ThreadManager;
use crate::thread_manager::ThreadSpawnResult;
use codex_agent_graph_store::AgentGraphStore;
use codex_agent_graph_store::AgentGraphStoreError;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_thread_store::InMemoryThreadStore;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[derive(Default)]
struct FaultGraph {
    edges: Mutex<HashMap<ThreadId, (ThreadId, ThreadSpawnEdgeStatus)>>,
    fail_closed: AtomicBool,
    pause_open: AtomicBool,
    open_started: Notify,
    release_open: Notify,
    revocations: AtomicUsize,
}

impl AgentGraphStore for FaultGraph {
    fn close_thread_spawn_edge_if_current(
        &self,
        expected: codex_agent_graph_store::ThreadSpawnEdgeAuthority,
        revoked_thread_ids: Vec<ThreadId>,
    ) -> AgentGraphStoreFuture<'_, bool> {
        Box::pin(async move {
            if self.fail_closed.load(Ordering::Acquire) {
                return Err(AgentGraphStoreError::Internal {
                    message: "injected close failure".to_string(),
                });
            }
            let mut edges = self.edges.lock().expect("graph");
            let edge = edges.get_mut(&expected.thread_id).ok_or_else(|| {
                AgentGraphStoreError::InvalidRequest {
                    message: "missing captured edge".to_string(),
                }
            })?;
            if edge.0 != expected.parent_thread_id
                || expected.owner_session_id.is_some()
                || !revoked_thread_ids.contains(&expected.thread_id)
            {
                return Err(AgentGraphStoreError::InvalidRequest {
                    message: "restoration edge authority changed".to_string(),
                });
            }
            let revoked = edge.1 == ThreadSpawnEdgeStatus::Open;
            if revoked {
                edge.1 = ThreadSpawnEdgeStatus::Closed;
                self.revocations.fetch_add(1, Ordering::AcqRel);
            }
            Ok(revoked)
        })
    }

    fn upsert_thread_spawn_edge(
        &self,
        parent: ThreadId,
        child: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.edges
                .lock()
                .expect("graph")
                .insert(child, (parent, status));
            Ok(())
        })
    }

    fn set_thread_spawn_edge_status(
        &self,
        child: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            if status == ThreadSpawnEdgeStatus::Closed && self.fail_closed.load(Ordering::Acquire) {
                return Err(AgentGraphStoreError::Internal {
                    message: "injected close failure".to_string(),
                });
            }
            if let Some(edge) = self.edges.lock().expect("graph").get_mut(&child) {
                edge.1 = status;
            }
            if status == ThreadSpawnEdgeStatus::Open && self.pause_open.load(Ordering::Acquire) {
                self.open_started.notify_one();
                self.release_open.notified().await;
            }
            Ok(())
        })
    }

    fn list_thread_spawn_children(
        &self,
        parent: ThreadId,
        filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            let mut children = self
                .edges
                .lock()
                .expect("graph")
                .iter()
                .filter_map(|(child, (owner, status))| {
                    (*owner == parent && filter.is_none_or(|filter| filter == *status))
                        .then_some(*child)
                })
                .collect::<Vec<_>>();
            children.sort_by_key(ToString::to_string);
            Ok(children)
        })
    }

    fn list_thread_spawn_descendants(
        &self,
        parent: ThreadId,
        filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.list_thread_spawn_children(parent, filter)
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    config: Config,
    manager: ThreadManager,
    owner: LocalAgentControl,
    parent: NewThread,
    child: ThreadSpawnResult,
    graph: Arc<FaultGraph>,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let mut config = crate::config::test_config().await;
    config.codex_home = home.path().to_path_buf().try_into().expect("absolute home");
    config.cwd = config.codex_home.clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let graph = Arc::new(FaultGraph::default());
    let manager = ThreadManager::new(
        &config,
        Arc::clone(&auth),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        codex_extension_api::empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        Arc::new(InMemoryThreadStore::default()),
        Some(graph.clone()),
        Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let parent = manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("start parent");
    let owner = parent.thread.session.services.agent_control.clone();
    let child = owner
        .upgrade()
        .expect("manager")
        .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
            ownership_override: None,
            registration: crate::thread_manager::ThreadRegistration::Deferred,
            config: config.clone(),
            initial_history: InitialHistory::New,
            agent_control: owner.clone(),
            session_source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: Some(AgentPath::root().join("worker").expect("child path")),
                agent_nickname: None,
                agent_role: None,
            }),
            parent_thread_id: Some(parent.thread_id),
            environment_selections: Some(Vec::new()),
            inherited_environments: None,
            inherited_instructions: None,
            inherited_exec_policy: None,
            client_mcp_extensions_override: Some(ClientMcpExtensions::default()),
        })
        .await
        .expect("prepare unpublished child");
    graph
        .upsert_thread_spawn_edge(
            parent.thread_id,
            child.thread_id,
            ThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("closed edge");
    Fixture {
        _home: home,
        config,
        manager,
        owner,
        parent,
        child,
        graph,
    }
}

#[tokio::test]
async fn failed_edge_rollback_fences_lazy_and_explicit_resume_until_acknowledged_close() {
    let fixture = fixture().await;
    let state = fixture.owner.upgrade().expect("manager");
    let generation = state.agent_lifecycle_generation(fixture.child.thread_id);
    let lock = state.v2_spawn_resume_lock(fixture.child.thread_id);
    let guard = lock.lock_owned().await;
    fixture.graph.fail_closed.store(true, Ordering::Release);
    let error = fixture
        .owner
        .publish_restored_agent(&fixture.child.thread, Some(&fixture.parent.thread), || {
            Err(CodexErr::InvalidRequest(
                "injected metadata failure".to_string(),
            ))
        })
        .await
        .expect_err("rollback should fail after open");
    let _error = fixture
        .owner
        .cleanup_unpublished_restoration(&fixture.child.thread, error)
        .await;
    drop(guard);
    assert!(
        fixture
            .manager
            .get_thread(fixture.child.thread_id)
            .await
            .is_err()
    );
    let fence = state
        .restoration_fence(fixture.child.thread_id)
        .expect("uncertain attempt fenced");
    assert_eq!(
        (fence.runtime, fence.parent_thread_id, fence.previous_edge),
        (
            fixture.child.thread.session.presentation_id(),
            fixture.parent.thread_id,
            ThreadSpawnEdgeStatus::Closed
        ),
    );
    let lazy_error = fixture
        .owner
        .ensure_v2_agent_loaded(fixture.config.clone(), fixture.child.thread_id)
        .await
        .err()
        .expect("lazy restoration fenced");
    let explicit_error = fixture
        .owner
        .resume_agent_from_rollout(
            fixture.config,
            fixture.child.thread_id,
            fixture.child.thread.session_source.clone(),
        )
        .await
        .err()
        .expect("explicit restoration fenced");
    for error in [lazy_error, explicit_error] {
        assert!(
            matches!(error.details(), CodexErrorDetails::InvalidRequest(message)
            if message.contains("restoration is quarantined"))
        );
    }
    let close_error = fixture
        .owner
        .close_agent(fixture.child.thread_id)
        .await
        .err()
        .expect("failed close keeps fence");
    assert!(close_error.to_string().contains("injected close failure"));
    assert_eq!(fixture.graph.revocations.load(Ordering::Acquire), 0);
    assert!(state.agent_lifecycle_generation_is_current(fixture.child.thread_id, generation));
    assert_eq!(
        state
            .restoration_fence(fixture.child.thread_id)
            .map(|fence| (fence.runtime, fence.parent_thread_id, fence.previous_edge,)),
        Some((fence.runtime, fence.parent_thread_id, fence.previous_edge)),
    );
    fixture.graph.fail_closed.store(false, Ordering::Release);
    fixture
        .owner
        .close_agent(fixture.child.thread_id)
        .await
        .expect("explicit close acknowledges edge");
    assert!(state.restoration_fence(fixture.child.thread_id).is_none());
    assert_eq!(fixture.graph.revocations.load(Ordering::Acquire), 1);
    assert!(!state.agent_lifecycle_generation_is_current(fixture.child.thread_id, generation));
    assert_eq!(
        fixture
            .graph
            .list_thread_spawn_children(
                fixture.parent.thread_id,
                Some(ThreadSpawnEdgeStatus::Closed)
            )
            .await
            .expect("closed children"),
        vec![fixture.child.thread_id],
    );
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
}

#[tokio::test]
async fn failed_publication_quarantines_an_edge_closed_during_metadata_commit() {
    let fixture = fixture().await;
    let state = fixture.owner.upgrade().expect("manager");
    let guard = state
        .agent_lifecycle_lock(fixture.child.thread_id)
        .lock_owned()
        .await;
    fixture
        .graph
        .upsert_thread_spawn_edge(
            fixture.parent.thread_id,
            fixture.child.thread_id,
            ThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("open edge before restoration");
    let error = fixture
        .owner
        .publish_restored_agent(&fixture.child.thread, Some(&fixture.parent.thread), || {
            fixture.graph.edges.lock().expect("graph").insert(
                fixture.child.thread_id,
                (fixture.parent.thread_id, ThreadSpawnEdgeStatus::Closed),
            );
            Err(CodexErr::InvalidRequest(
                "injected publication failure".to_string(),
            ))
        })
        .await
        .expect_err("changed edge is quarantined");
    assert!(
        error
            .to_string()
            .contains("spawn edge changed during publication")
    );
    let _error = fixture
        .owner
        .cleanup_unpublished_restoration(&fixture.child.thread, error)
        .await;
    drop(guard);
    assert_eq!(
        fixture
            .graph
            .edges
            .lock()
            .expect("graph")
            .get(&fixture.child.thread_id)
            .copied(),
        Some((fixture.parent.thread_id, ThreadSpawnEdgeStatus::Closed)),
    );
    assert_eq!(fixture.graph.revocations.load(Ordering::Acquire), 0);
    assert!(state.restoration_fence(fixture.child.thread_id).is_some());
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
}

#[tokio::test]
async fn stale_restoration_fence_cannot_close_a_reparented_edge() {
    let fixture = fixture().await;
    let state = fixture.owner.upgrade().expect("manager");
    let guard = state
        .agent_lifecycle_lock(fixture.child.thread_id)
        .lock_owned()
        .await;
    fixture.graph.fail_closed.store(true, Ordering::Release);
    let error = fixture
        .owner
        .publish_restored_agent(&fixture.child.thread, Some(&fixture.parent.thread), || {
            Err(CodexErr::InvalidRequest(
                "injected publication failure".to_string(),
            ))
        })
        .await
        .expect_err("failed rollback establishes quarantine");
    let _error = fixture
        .owner
        .cleanup_unpublished_restoration(&fixture.child.thread, error)
        .await;
    drop(guard);
    let new_parent = ThreadId::new();
    fixture.graph.edges.lock().expect("graph").insert(
        fixture.child.thread_id,
        (new_parent, ThreadSpawnEdgeStatus::Open),
    );
    fixture.graph.fail_closed.store(false, Ordering::Release);
    let generation = state.agent_lifecycle_generation(fixture.child.thread_id);
    let error = fixture
        .owner
        .close_agent(fixture.child.thread_id)
        .await
        .expect_err("stale quarantine cannot mutate adopted edge");
    assert!(error.to_string().contains("authority changed"));
    assert_eq!(
        fixture
            .graph
            .edges
            .lock()
            .expect("graph")
            .get(&fixture.child.thread_id)
            .copied(),
        Some((new_parent, ThreadSpawnEdgeStatus::Open)),
    );
    assert_eq!(fixture.graph.revocations.load(Ordering::Acquire), 0);
    assert!(state.agent_lifecycle_generation_is_current(fixture.child.thread_id, generation));
    assert!(state.restoration_fence(fixture.child.thread_id).is_some());
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
}

#[tokio::test]
async fn cancelled_resume_caller_does_not_abandon_runtime_after_deferred_start() {
    let fixture = fixture().await;
    fixture.child.thread.ensure_rollout_materialized().await;
    fixture
        .child
        .thread
        .session
        .flush_rollout()
        .await
        .expect("flush child");
    fixture
        .child
        .thread
        .shutdown_and_wait()
        .await
        .expect("close first writer");
    let state = fixture.owner.upgrade().expect("manager");
    let stored = state
        .read_stored_thread(ReadThreadParams {
            thread_id: fixture.child.thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .expect("stored child");
    let history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: fixture.child.thread_id,
        history: Arc::new(stored.history.expect("child history").items),
        rollout_path: stored.rollout_path,
    });
    fixture.graph.pause_open.store(true, Ordering::Release);
    let manager = Arc::new(fixture.manager);
    let caller = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move {
            manager
                .resume_thread_with_history(
                    fixture.config,
                    history,
                    AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
                    /*parent_trace*/ None,
                    ClientMcpExtensions::default(),
                )
                .await
        }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 5),
        fixture.graph.open_started.notified(),
    )
    .await
    .expect("resume reached graph write after deferred startup");
    assert!(manager.get_thread(fixture.child.thread_id).await.is_err());
    assert!(
        state
            .v2_spawn_resume_lock(fixture.child.thread_id)
            .try_lock_owned()
            .is_err()
    );
    caller.abort();
    assert!(caller.await.err().expect("caller cancelled").is_cancelled());
    fixture.graph.release_open.notify_one();
    let restored = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
        loop {
            if let Ok(thread) = manager.get_thread(fixture.child.thread_id).await {
                break thread;
            }
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("owned restoration worker completed publication");
    assert_eq!(restored.session_source, fixture.child.thread.session_source);
    restored
        .shutdown_and_wait()
        .await
        .expect("shutdown restored child");
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
}
