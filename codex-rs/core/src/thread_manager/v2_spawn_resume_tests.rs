use super::*;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use pretty_assertions::assert_eq;

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

async fn deferred_publication_fixture() -> (tempfile::TempDir, ThreadManager, NewThread, NewThread)
{
    let home = tempfile::tempdir().expect("temporary home");
    let mut config = crate::config::test_config().await;
    config.codex_home = home.path().to_path_buf().try_into().expect("absolute home");
    config.cwd = config.codex_home.clone();
    config.ephemeral = true;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let parent = manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("start parent");
    let mut request = ThreadSpawnRequest::new(
        StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config)
        },
        Arc::clone(&manager.state.auth_manager),
        parent.thread.session.services.agent_control.clone(),
    );
    request.registration = ThreadRegistration::Deferred;
    let child = manager
        .state
        .spawn_thread(request)
        .await
        .expect("prepare child");
    assert!(manager.get_thread(child.thread_id).await.is_err());
    (home, manager, parent, child)
}

#[tokio::test]
async fn deferred_publication_preserves_absence_on_metadata_failure_then_commits_exact_runtime() {
    let (_home, manager, parent, child) = deferred_publication_fixture().await;
    let error = manager
        .state
        .publish_restored_thread(&child.thread, Some(&parent.thread), || {
            Err(CodexErr::InvalidRequest(
                "stale metadata reservation".to_string(),
            ))
        })
        .await
        .expect_err("metadata failure must prevent publication");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    assert!(manager.get_thread(child.thread_id).await.is_err());
    let committed = std::sync::atomic::AtomicBool::new(false);
    manager
        .state
        .publish_restored_thread(&child.thread, Some(&parent.thread), || {
            committed.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .expect("publish exact prepared child");
    assert!(committed.load(Ordering::Acquire));
    assert!(Arc::ptr_eq(
        &manager
            .get_thread(child.thread_id)
            .await
            .expect("published child"),
        &child.thread,
    ));
    child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown child");
    parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
}

#[tokio::test]
async fn deferred_publication_rechecks_parent_after_waiting_for_manager_write_lock() {
    let (_home, manager, parent, child) = deferred_publication_fixture().await;
    let committed = std::sync::atomic::AtomicBool::new(false);
    let mut threads = manager.state.threads.write().await;
    let publication =
        manager
            .state
            .publish_restored_thread(&child.thread, Some(&parent.thread), || {
                committed.store(true, Ordering::Release);
                Ok(())
            });
    tokio::pin!(publication);
    assert!(futures::poll!(&mut publication).is_pending());
    // Simulate removal immediately before the queued publication acquires the map lock.
    threads.remove(&parent.thread_id);
    drop(threads);
    assert!(publication.await.is_err());
    assert!(!committed.load(Ordering::Acquire));
    assert!(manager.get_thread(child.thread_id).await.is_err());
    child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown unpublished child");
    parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown removed parent");
}

#[tokio::test]
async fn exact_removal_and_absent_cleanup_never_mutate_a_replacement_runtime() {
    let (_home, manager, parent, child) = deferred_publication_fixture().await;
    let removed = manager
        .state
        .remove_thread_if_matches_with(&parent.thread_id, &child.thread, || {
            panic!("replacement metadata must be retained")
        })
        .await;
    assert!(removed.is_none());
    manager
        .state
        .run_if_thread_absent(parent.thread_id, || {
            panic!("live runtime metadata must be retained");
        })
        .await;
    assert!(Arc::ptr_eq(
        &manager
            .get_thread(parent.thread_id)
            .await
            .expect("original parent"),
        &parent.thread,
    ));
    child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown unpublished child");
    parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
}
