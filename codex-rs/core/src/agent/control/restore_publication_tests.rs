use super::*;
use crate::thread_manager::NewThread;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::ThreadManager;
use codex_agent_graph_store::AgentGraphStore;
use codex_agent_graph_store::AgentGraphStoreError;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadReadyInput;
use codex_extension_api::ThreadResumeInput;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_thread_store::InMemoryThreadStore;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[derive(Default)]
struct FaultGraph {
    edges: Mutex<HashMap<ThreadId, (ThreadId, ThreadSpawnEdgeStatus)>>,
    fail_closed: AtomicBool,
    pause_open: AtomicBool,
    open_started: Notify,
    release_open: Notify,
}

impl AgentGraphStore for FaultGraph {
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

#[derive(Default)]
struct LifecycleRecord {
    events: Mutex<Vec<(String, &'static str)>>,
    changed: Notify,
}

impl LifecycleRecord {
    fn record(&self, thread_id: &str, phase: &'static str) {
        self.events
            .lock()
            .expect("lifecycle events")
            .push((thread_id.to_owned(), phase));
        self.changed.notify_one();
    }

    fn for_thread(&self, thread_id: ThreadId) -> Vec<&'static str> {
        let thread_id = thread_id.to_string();
        self.events
            .lock()
            .expect("lifecycle events")
            .iter()
            .filter(|(id, _)| id == &thread_id)
            .map(|(_, phase)| *phase)
            .collect()
    }
}

impl ThreadLifecycleContributor<Config> for LifecycleRecord {
    fn on_thread_ready<'a>(
        &'a self,
        input: ThreadReadyInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.record(input.thread_store.level_id(), "ready");
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.record(input.thread_store.level_id(), "resume");
        })
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    config: Config,
    manager: ThreadManager,
    owner: LocalAgentControl,
    parent: NewThread,
    child: NewThread,
    graph: Arc<FaultGraph>,
    lifecycle: Arc<LifecycleRecord>,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let mut config = crate::config::test_config().await;
    config.codex_home = home.path().to_path_buf().try_into().expect("absolute home");
    config.cwd = config.codex_home.clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let graph = Arc::new(FaultGraph::default());
    let lifecycle = Arc::new(LifecycleRecord::default());
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(lifecycle.clone());
    let manager = ThreadManager::new(
        &config,
        Arc::clone(&auth),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(extensions.build()),
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
    let owner = parent
        .thread
        .session
        .services
        .local_agent_runtime
        .control(parent.thread.session.session_id());
    // This fixture bypasses the tool resolver that normally registers the root.
    owner
        .runtime
        .register_session_root(parent.thread_id, /*current_parent_thread_id*/ None);
    let child = owner
        .runtime
        .upgrade()
        .expect("manager")
        .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
            prompt_cache_key: None,
            registration: crate::thread_manager::ThreadRegistrationMode::Deferred,
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
        .expect("prepare unpublished child")
        .into_new_thread();
    assert_eq!(lifecycle.for_thread(parent.thread_id), vec!["ready"]);
    assert!(lifecycle.for_thread(child.thread_id).is_empty());
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
        lifecycle,
    }
}

#[tokio::test]
async fn failed_edge_rollback_fences_lazy_and_explicit_resume_until_acknowledged_close() {
    let fixture = fixture().await;
    let state = fixture.owner.runtime.upgrade().expect("manager");
    let lock = state.v2_spawn_resume_lock(fixture.child.thread_id);
    let guard = lock.lock_owned().await;
    fixture.graph.fail_closed.store(true, Ordering::Release);
    let error = fixture
        .owner
        .publish_restored_agent(
            &fixture.child.thread,
            Some(&fixture.parent.thread),
            /*resumed_created_at*/ None,
            || {
                Err(CodexErr::InvalidRequest(
                    "injected metadata failure".to_string(),
                ))
            },
        )
        .await
        .expect_err("rollback should fail after open");
    let _error = fixture
        .owner
        .cleanup_unpublished_restoration(&fixture.child.thread, error)
        .await;
    drop(guard);
    assert!(
        fixture
            .lifecycle
            .for_thread(fixture.child.thread_id)
            .is_empty()
    );
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
        .ensure_v2_agent_loaded(
            fixture.config.clone(),
            fixture.child.thread_id,
            /*parent*/ None,
        )
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
    let state = fixture.owner.runtime.upgrade().expect("manager");
    let stored = state
        .read_stored_thread(ReadThreadParams {
            thread_id: fixture.child.thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .expect("stored child");
    let context = stored.history.expect("child history");
    let history = InitialHistory::Resumed(ResumedHistory {
        history_revision: context.revision,
        conversation_id: fixture.child.thread_id,
        history: Arc::new(context.items),
        rollout_path: stored.rollout_path,
    });
    fixture.graph.pause_open.store(true, Ordering::Release);
    let refresh_config = fixture.config.clone();
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
        fixture
            .lifecycle
            .for_thread(fixture.child.thread_id)
            .is_empty()
    );
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
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
        loop {
            let changed = fixture.lifecycle.changed.notified();
            if fixture.lifecycle.for_thread(fixture.child.thread_id).len() >= 2 {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("owned publication emits both lifecycle callbacks after caller cancellation");
    assert_eq!(
        fixture.lifecycle.for_thread(fixture.child.thread_id),
        vec!["ready", "resume"],
    );
    let pin = fixture
        .owner
        .ensure_v2_agent_loaded(
            refresh_config,
            fixture.child.thread_id,
            Some(Arc::clone(&fixture.parent.thread)),
        )
        .await
        .expect("refresh the already published exact runtime");
    assert!(Arc::ptr_eq(
        &restored,
        &manager
            .get_thread(fixture.child.thread_id)
            .await
            .expect("registered child"),
    ));
    assert_eq!(
        fixture.lifecycle.for_thread(fixture.child.thread_id),
        vec!["ready", "resume"],
    );
    drop(pin);
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

#[tokio::test]
async fn captured_restore_owner_keeps_lifecycle_and_rejects_removed_or_foreign_parent() {
    let fixture = fixture().await;
    let manager = fixture.owner.runtime.upgrade().expect("manager");
    let parent_id = fixture.parent.thread_id;
    let parent = &fixture.parent.thread;
    let foreign = fixture
        .manager
        .agent_control()
        .runtime
        .control(parent.session.session_id());
    assert!(
        foreign
            .capture_restoration_owner(
                &manager,
                Some(parent_id),
                Some(Arc::clone(parent)),
                MultiAgentVersion::V2,
            )
            .await
            .is_err()
    );
    let busy = manager.v2_spawn_resume_lock(parent_id).lock_owned().await;
    assert!(
        fixture
            .owner
            .capture_restoration_owner(
                &manager,
                Some(parent_id),
                Some(Arc::clone(parent)),
                MultiAgentVersion::V2,
            )
            .await
            .is_err()
    );
    drop(busy);
    let captured = fixture
        .owner
        .capture_restoration_owner(
            &manager,
            Some(parent_id),
            Some(Arc::clone(parent)),
            MultiAgentVersion::V2,
        )
        .await
        .expect("capture the actual owner");
    assert!(Arc::ptr_eq(
        captured.parent.as_ref().expect("parent"),
        parent
    ));
    assert!(
        manager
            .v2_spawn_resume_lock(parent_id)
            .try_lock_owned()
            .is_err()
    );
    let removed = manager
        .remove_thread_if_matches(&parent_id, parent)
        .await
        .expect("remove exact parent");
    assert!(Arc::ptr_eq(&removed, parent));
    let metadata_committed = AtomicBool::new(false);
    assert!(
        manager
            .publish_restored_thread(&fixture.child.thread, captured.parent.as_ref(), || {
                metadata_committed.store(true, Ordering::Release);
                Ok(())
            })
            .await
            .is_err()
    );
    assert!(!metadata_committed.load(Ordering::Acquire));
    assert!(manager.get_thread(fixture.child.thread_id).await.is_err());
    drop(captured);
    assert!(
        manager
            .v2_spawn_resume_lock(parent_id)
            .try_lock_owned()
            .is_ok()
    );
    fixture
        .child
        .thread
        .shutdown_and_wait()
        .await
        .expect("close unpublished child");
    parent
        .shutdown_and_wait()
        .await
        .expect("close removed parent");
}
