use super::*;
use crate::config::test_config;
use codex_agent_graph_store::AgentAliasState;
use codex_agent_graph_store::AgentGraphStoreError;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_agent_graph_store::AllocateAgentAliasRequest;
use codex_agent_graph_store::TransferAgentAliasRequest;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

struct EmptyAgentGraphStore;

impl AgentGraphStore for EmptyAgentGraphStore {
    fn upsert_thread_spawn_edge(
        &self,
        _parent_thread_id: ThreadId,
        _child_thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn set_thread_spawn_edge_status(
        &self,
        _child_thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn find_thread_spawn_parent(
        &self,
        _child_thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<ThreadId>> {
        Box::pin(async { Ok(None) })
    }

    fn list_thread_spawn_children(
        &self,
        _parent_thread_id: ThreadId,
        _status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn list_thread_spawn_descendants(
        &self,
        _root_thread_id: ThreadId,
        _status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

struct LifecycleRestoreFailureStore;

impl AgentGraphStore for LifecycleRestoreFailureStore {
    fn supports_agent_aliases(&self) -> bool {
        true
    }

    fn find_agent_alias_by_thread(
        &self,
        _session_id: SessionId,
        _thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<codex_agent_graph_store::AgentAlias>> {
        Box::pin(async {
            Err(AgentGraphStoreError::Internal {
                message: "simulated lifecycle read failure".to_string(),
            })
        })
    }

    fn upsert_thread_spawn_edge(
        &self,
        _parent_thread_id: ThreadId,
        _child_thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn set_thread_spawn_edge_status(
        &self,
        _child_thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn find_thread_spawn_parent(
        &self,
        _child_thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<ThreadId>> {
        Box::pin(async { Ok(None) })
    }

    fn list_thread_spawn_children(
        &self,
        _parent_thread_id: ThreadId,
        _status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn list_thread_spawn_descendants(
        &self,
        _root_thread_id: ThreadId,
        _status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn resumed_v2_history(
    child_thread_id: ThreadId,
    session_id: SessionId,
    source: SessionSource,
) -> InitialHistory {
    InitialHistory::Resumed(ResumedHistory {
        conversation_id: child_thread_id,
        history: Arc::new(vec![RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id,
                id: child_thread_id,
                source,
                multi_agent_version: Some(MultiAgentVersion::V2),
                ..SessionMeta::default()
            },
            git: None,
        })]),
        rollout_path: None,
    })
}

fn assert_invalid_request(
    result: CodexResult<Option<PersistedV2SpawnResume>>,
    expected_message: &str,
) {
    match result {
        Err(err) => match err.details() {
            CodexErrorDetails::InvalidRequest(message) => assert!(
                message.contains(expected_message),
                "unexpected invalid-request message: {message}"
            ),
            _ => panic!("expected invalid request, got {err}"),
        },
        Ok(_) => panic!("expected spawned V2 history to reject detached fallback"),
    }
}

#[tokio::test]
async fn spawned_v2_history_without_graph_rejects_detached_fallback() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let history = resumed_v2_history(
        child_thread_id,
        parent_thread_id.into(),
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );

    assert_invalid_request(
        resolve_persisted_v2_spawn_resume(&history, None).await,
        "persisted agent graph is unavailable",
    );
}

#[tokio::test]
async fn spawned_v2_history_uses_latest_exact_session_metadata() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let session_id = SessionId::from(parent_thread_id);
    let history = resumed_v2_history(
        child_thread_id,
        session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );
    let InitialHistory::Resumed(mut resumed) = history else {
        panic!("test history should be resumed");
    };
    let mut items = resumed.history.as_ref().clone();
    items.insert(
        0,
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id,
                id: child_thread_id,
                source: SessionSource::default(),
                multi_agent_version: Some(MultiAgentVersion::V2),
                ..SessionMeta::default()
            },
            git: None,
        }),
    );
    resumed.history = Arc::new(items);
    let latest_sources = InitialHistory::Resumed(resumed.clone()).get_resumed_session_sources();

    assert_invalid_request(
        resolve_persisted_v2_spawn_resume(&InitialHistory::Resumed(resumed), None).await,
        "persisted agent graph is unavailable",
    );
    assert_eq!(
        latest_sources,
        Some((
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            None,
        ))
    );
}

#[tokio::test]
async fn latest_non_subagent_metadata_keeps_generic_resume_identity() {
    let stale_parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let session_id = SessionId::from(stale_parent_thread_id);
    let history = resumed_v2_history(
        child_thread_id,
        session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: stale_parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );
    let InitialHistory::Resumed(mut resumed) = history else {
        panic!("test history should be resumed");
    };
    let mut items = resumed.history.as_ref().clone();
    items.push(RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            session_id,
            id: child_thread_id,
            source: SessionSource::Exec,
            multi_agent_version: Some(MultiAgentVersion::V2),
            ..SessionMeta::default()
        },
        git: None,
    }));
    resumed.history = Arc::new(items);
    let history = InitialHistory::Resumed(resumed);

    let result = resolve_persisted_v2_spawn_resume(&history, None)
        .await
        .expect("latest arbitrary metadata should remain eligible for generic resume");

    assert!(result.is_none());
    assert_eq!(
        history.get_resumed_session_sources(),
        Some((SessionSource::Exec, None))
    );
}

#[tokio::test]
async fn spawned_v2_history_without_edge_rejects_detached_fallback() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let history = resumed_v2_history(
        child_thread_id,
        parent_thread_id.into(),
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );
    let agent_graph_store: Arc<dyn AgentGraphStore> = Arc::new(EmptyAgentGraphStore);

    assert_invalid_request(
        resolve_persisted_v2_spawn_resume(&history, Some(agent_graph_store)).await,
        "persisted thread-spawn edge",
    );
}

#[tokio::test]
async fn arbitrary_v2_history_without_graph_keeps_generic_resume_semantics() {
    let thread_id = ThreadId::new();
    let history = resumed_v2_history(thread_id, thread_id.into(), SessionSource::default());

    let result = resolve_persisted_v2_spawn_resume(&history, None)
        .await
        .expect("arbitrary V2 history should remain eligible for generic resume");

    assert!(result.is_none());
}

#[tokio::test]
async fn transferred_v2_subtree_resolves_current_owner_parent_and_depth() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let state_db = crate::init_state_db(&config).await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(EnvironmentManager::default_for_tests()),
        state_db,
    );
    let graph = manager
        .state
        .agent_graph_store()
        .expect("agent graph store");
    let source_root = ThreadId::new();
    let destination_root = ThreadId::new();
    let child = ThreadId::new();
    let descendant = ThreadId::new();
    let source_session_id = SessionId::from(source_root);
    let destination_session_id = SessionId::from(destination_root);
    for session_id in [source_session_id, destination_session_id] {
        graph
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("initialize alias namespace");
    }
    graph
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root,
            child_thread_id: child,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("allocate child alias");
    graph
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: child,
            child_thread_id: descendant,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("allocate descendant alias");
    graph
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source_session_id),
            expected_descendant_thread_ids: vec![descendant],
            new_session_id: destination_session_id,
            new_parent_thread_id: destination_root,
            thread_id: child,
            nickname: Some("Hopper".to_string()),
            authored_selector: child.to_string(),
        })
        .await
        .expect("transfer subtree");

    let child_history = resumed_v2_history(
        child,
        source_session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: source_root,
            depth: 1,
            agent_path: Some(AgentPath::root().join("old_child").expect("old child path")),
            agent_nickname: Some("Hopper".to_string()),
            agent_role: Some("worker".to_string()),
        }),
    );
    let descendant_history = resumed_v2_history(
        descendant,
        source_session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: child,
            depth: 2,
            agent_path: Some(
                AgentPath::root()
                    .join("old_child")
                    .and_then(|path| path.join("old_descendant"))
                    .expect("old descendant path"),
            ),
            agent_nickname: Some("Noether".to_string()),
            agent_role: Some("reviewer".to_string()),
        }),
    );

    let child_resume = resolve_persisted_v2_spawn_resume(&child_history, Some(Arc::clone(&graph)))
        .await
        .expect("resolve transferred child")
        .expect("transferred child remains a V2 spawn");
    let descendant_resume = resolve_persisted_v2_spawn_resume(&descendant_history, Some(graph))
        .await
        .expect("resolve transferred descendant")
        .expect("transferred descendant remains a V2 spawn");

    assert_eq!(
        (
            child_resume.session_id,
            child_resume.parent_thread_id,
            child_resume.edge_status,
            child_resume.session_source,
        ),
        (
            destination_session_id,
            destination_root,
            ThreadSpawnEdgeStatus::Open,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: destination_root,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Hopper".to_string()),
                agent_role: Some("worker".to_string()),
            }),
        )
    );
    assert_eq!(
        (
            descendant_resume.session_id,
            descendant_resume.parent_thread_id,
            descendant_resume.edge_status,
            descendant_resume.session_source,
        ),
        (
            destination_session_id,
            child,
            ThreadSpawnEdgeStatus::Open,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child,
                depth: 2,
                agent_path: None,
                agent_nickname: Some("Noether".to_string()),
                agent_role: Some("reviewer".to_string()),
            }),
        )
    );
}

#[tokio::test]
async fn cold_v2_resume_relocks_parent_after_concurrent_transfer() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let state_db = crate::init_state_db(&config).await;
    let manager = Arc::new(
        ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            Arc::new(EnvironmentManager::default_for_tests()),
            state_db,
        ),
    );
    let graph = manager
        .state
        .agent_graph_store()
        .expect("agent graph store");
    let source_root = ThreadId::new();
    let destination_root = ThreadId::new();
    let child = ThreadId::new();
    let source_session_id = SessionId::from(source_root);
    let destination_session_id = SessionId::from(destination_root);
    for session_id in [source_session_id, destination_session_id] {
        graph
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("initialize alias namespace");
    }
    graph
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root,
            child_thread_id: child,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("allocate source child alias");
    let history = resumed_v2_history(
        child,
        source_session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: source_root,
            depth: 1,
            agent_path: None,
            agent_nickname: Some("Hopper".to_string()),
            agent_role: Some("worker".to_string()),
        }),
    );

    let child_lifecycle_guard = manager.state.agent_lifecycle_lock(child).lock_owned().await;
    let destination_parent_guard = manager
        .state
        .agent_lifecycle_lock(destination_root)
        .lock_owned()
        .await;
    let source_parent_lock = manager.state.agent_lifecycle_lock(source_root);
    let manager_for_resume = Arc::clone(&manager);
    let config_for_resume = config.clone();
    let mut resume = tokio::spawn(async move {
        manager_for_resume
            .try_resume_persisted_v2_spawn(
                &config_for_resume,
                &history,
                &ClientMcpExtensions::default(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 1), async {
        loop {
            if source_parent_lock.clone().try_lock_owned().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cold resume should hold the initially resolved parent");

    graph
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: destination_session_id,
            new_parent_thread_id: destination_root,
            thread_id: child,
            nickname: Some("Hopper".to_string()),
            authored_selector: child.to_string(),
        })
        .await
        .expect("transfer child while cold resume waits for its lifecycle lock");
    drop(child_lifecycle_guard);

    tokio::time::timeout(Duration::from_secs(/*secs*/ 1), async {
        loop {
            if let Ok(source_parent_guard) = source_parent_lock.clone().try_lock_owned() {
                drop(source_parent_guard);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cold resume should release the stale parent before retrying");
    assert!(
        tokio::time::timeout(Duration::from_millis(/*millis*/ 100), &mut resume)
            .await
            .is_err(),
        "cold resume must retry and wait for the transferred destination parent"
    );
    drop(destination_parent_guard);
    let error = resume
        .await
        .expect("cold resume task")
        .expect_err("destination parent is intentionally not loaded");
    assert!(
        error
            .to_string()
            .contains(&format!("direct parent {destination_root} is not loaded")),
        "cold resume should validate the re-resolved parent: {error}"
    );
}

#[tokio::test]
async fn failed_v2_validation_preserves_an_adopted_runtime_and_restores_its_metadata() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(EnvironmentManager::default_for_tests()),
    );
    let running = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .expect("start runtime owned by the first resume caller");
    let owner = manager.agent_control();
    let original_metadata = crate::agent::AgentMetadata {
        agent_id: Some(running.thread_id),
        agent_nickname: Some("original".to_string()),
        agent_role: Some("worker".to_string()),
        ..Default::default()
    };
    owner
        .restore_agent_metadata(running.thread_id, original_metadata.clone())
        .expect("register original metadata");
    let attempt_metadata = crate::agent::AgentMetadata {
        agent_id: Some(running.thread_id),
        agent_nickname: Some("temporary".to_string()),
        agent_role: Some("reviewer".to_string()),
        ..Default::default()
    };
    owner
        .restore_agent_metadata(running.thread_id, attempt_metadata.clone())
        .expect("simulate metadata changed by the adopting resume");

    // A second resume caller can discover this already-running thread, then fail a later
    // validation step. Its rollback owns only the metadata mutation above, not the runtime
    // created by the first caller.
    let error = cleanup_failed_v2_spawn_resume(
        &manager.state,
        &owner,
        &running.thread,
        ThreadRuntimeOrigin::Existing,
        Some(&original_metadata),
        Some(&attempt_metadata),
        /*lifecycle_restore*/ None,
        CodexErr::InvalidRequest("simulated post-adoption validation failure".to_string()),
    )
    .await;

    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "simulated post-adoption validation failure"
    ));
    let retained = manager
        .get_thread(running.thread_id)
        .await
        .expect("failed adopting resume must preserve the first caller's runtime");
    assert!(Arc::ptr_eq(&retained, &running.thread));
    assert_eq!(
        owner.get_agent_metadata(running.thread_id),
        Some(original_metadata.clone())
    );

    owner
        .restore_agent_metadata(running.thread_id, attempt_metadata.clone())
        .expect("restore attempt metadata for lost-update coverage");
    let concurrent_metadata = crate::agent::AgentMetadata {
        agent_id: Some(running.thread_id),
        agent_nickname: Some("newer".to_string()),
        agent_role: Some("worker".to_string()),
        last_task_message: Some("new task from a concurrent sender".to_string()),
        ..Default::default()
    };
    owner
        .restore_agent_metadata(running.thread_id, concurrent_metadata.clone())
        .expect("simulate a concurrent authoritative metadata update");
    let _ = cleanup_failed_v2_spawn_resume(
        &manager.state,
        &owner,
        &running.thread,
        ThreadRuntimeOrigin::Existing,
        Some(&original_metadata),
        Some(&attempt_metadata),
        /*lifecycle_restore*/ None,
        CodexErr::InvalidRequest("simulated validation failure after newer update".to_string()),
    )
    .await;
    assert_eq!(
        owner.get_agent_metadata(running.thread_id),
        Some(concurrent_metadata)
    );

    let _ = manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;
}

#[tokio::test]
async fn failed_closed_v2_resume_restores_alias_and_edge_lifecycle() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::MultiAgentV2)
        .expect("enable multi-agent v2");
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let state_db = crate::init_state_db(&config).await;
    let root_thread_id =
        ThreadId::from_u128(/*value*/ 0x018f_0000_0000_7000_8000_0000_0000_0031);
    let child_thread_id =
        ThreadId::from_u128(/*value*/ 0x018f_0000_0000_7000_8000_0000_0000_0032);
    let generated_ids = [root_thread_id, child_thread_id];
    let next_id = std::sync::atomic::AtomicUsize::new(0);
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(EnvironmentManager::default_for_tests()),
        state_db,
    )
    .with_thread_id_generator(move || generated_ids[next_id.fetch_add(1, Ordering::Relaxed)]);
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root");
    let owner = root.thread.session.services.agent_control.clone();
    let agent_graph_store = manager
        .state
        .agent_graph_store()
        .expect("agent graph store");
    agent_graph_store
        .ensure_agent_alias_namespace(SessionId::from(root_thread_id))
        .await
        .expect("initialize root aliases");
    agent_graph_store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: SessionId::from(root_thread_id),
            parent_thread_id: root_thread_id,
            child_thread_id,
            nickname: Some("worker".to_string()),
        })
        .await
        .expect("allocate child alias");
    assert!(
        agent_graph_store
            .set_agent_lifecycle_state(
                SessionId::from(root_thread_id),
                child_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("close child alias")
    );
    agent_graph_store
        .activate_agent_alias(AllocateAgentAliasRequest {
            session_id: SessionId::from(root_thread_id),
            parent_thread_id: root_thread_id,
            child_thread_id,
            nickname: Some("worker".to_string()),
        })
        .await
        .expect("simulate resume activation");
    let hidden_child = manager
        .state
        .spawn_new_thread_with_source(
            config,
            owner.clone(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("worker".to_string()),
                agent_role: Some("worker".to_string()),
            }),
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(root_thread_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            ThreadRuntimePublication::Deferred,
        )
        .await
        .expect("create unpublished resumed runtime");

    let error = cleanup_failed_v2_spawn_resume(
        &manager.state,
        &owner,
        &hidden_child.thread,
        ThreadRuntimeOrigin::Created,
        /*previous_metadata*/ None,
        /*attempt_metadata*/ None,
        Some((
            &agent_graph_store,
            SessionId::from(root_thread_id),
            child_thread_id,
            ThreadSpawnEdgeStatus::Closed,
        )),
        CodexErr::InvalidRequest("simulated post-activation failure".to_string()),
    )
    .await;

    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "simulated post-activation failure"
    ));
    assert_eq!(
        agent_graph_store
            .find_agent_alias_by_thread(SessionId::from(root_thread_id), child_thread_id)
            .await
            .expect("read restored alias")
            .map(|alias| alias.state),
        Some(AgentAliasState::Closed)
    );
    assert_eq!(
        agent_graph_store
            .list_thread_spawn_children(
                root_thread_id,
                /*status_filter*/ Some(ThreadSpawnEdgeStatus::Closed),
            )
            .await
            .expect("read restored edge"),
        vec![child_thread_id]
    );
    root.thread
        .shutdown_and_wait()
        .await
        .expect("shutdown root");
}

#[tokio::test]
async fn lifecycle_restore_failure_does_not_skip_unpublished_runtime_cleanup() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root");
    let owner = root.thread.session.services.agent_control.clone();
    let hidden_child = manager
        .state
        .spawn_new_thread_with_source(
            config,
            owner.clone(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("worker".to_string()),
                agent_role: Some("worker".to_string()),
            }),
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(root.thread_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            ThreadRuntimePublication::Deferred,
        )
        .await
        .expect("create unpublished resumed runtime");
    owner
        .restore_agent_metadata(
            hidden_child.thread_id,
            crate::agent::AgentMetadata {
                agent_id: Some(hidden_child.thread_id),
                agent_nickname: Some("worker".to_string()),
                ..Default::default()
            },
        )
        .expect("register unpublished child metadata");
    let lifecycle_store: Arc<dyn AgentGraphStore> = Arc::new(LifecycleRestoreFailureStore);

    let error = cleanup_failed_v2_spawn_resume(
        &manager.state,
        &owner,
        &hidden_child.thread,
        ThreadRuntimeOrigin::Created,
        /*previous_metadata*/ None,
        /*attempt_metadata*/ None,
        Some((
            &lifecycle_store,
            SessionId::from(root.thread_id),
            hidden_child.thread_id,
            ThreadSpawnEdgeStatus::Closed,
        )),
        CodexErr::InvalidRequest("simulated post-activation failure".to_string()),
    )
    .await;

    assert!(matches!(
        error.details(),
        CodexErrorDetails::Fatal(message)
            if message.contains("simulated post-activation failure")
                && message.contains("simulated lifecycle read failure")
    ));
    assert!(
        manager
            .state
            .get_thread_including_pending(hidden_child.thread_id)
            .await
            .is_err(),
        "lifecycle rollback failure must not retain the setup-pending runtime"
    );
    assert_eq!(owner.get_agent_metadata(hidden_child.thread_id), None);
    root.thread
        .shutdown_and_wait()
        .await
        .expect("shutdown root");
}
