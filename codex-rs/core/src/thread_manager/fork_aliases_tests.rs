use super::*;
use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentGraphStore;
use codex_agent_graph_store::AgentGraphStoreError;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_agent_graph_store::AllocateAgentAliasRequest;
use codex_agent_graph_store::ReserveForkAgentAliasesRequest;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::SessionId;
use codex_thread_store::InMemoryThreadStore;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use test_case::test_case;

/// Gates a real SQLite reservation after commit, including ambiguous failure and retry.
struct GatedForkGraph {
    inner: Arc<dyn AgentGraphStore>,
    reserved: Notify,
    release: Notify,
    discard_failed: Notify,
    fail_reservation: bool,
    fail_discard: AtomicBool,
}

impl AgentGraphStore for GatedForkGraph {
    fn supports_agent_aliases(&self) -> bool {
        true
    }

    fn ensure_agent_alias_namespace(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, AgentAlias> {
        self.inner.ensure_agent_alias_namespace(session_id)
    }

    fn find_current_agent_alias_by_thread(
        &self,
        thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<AgentAlias>> {
        self.inner.find_current_agent_alias_by_thread(thread_id)
    }

    fn find_thread_spawn_parent(
        &self,
        child_thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<ThreadId>> {
        self.inner.find_thread_spawn_parent(child_thread_id)
    }

    fn reserve_agent_aliases_for_fork(
        &self,
        request: ReserveForkAgentAliasesRequest,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.inner.reserve_agent_aliases_for_fork(request).await?;
            self.reserved.notify_one();
            self.release.notified().await;
            if self.fail_reservation {
                return Err(AgentGraphStoreError::Internal {
                    message: "reservation acknowledgement lost".to_string(),
                });
            }
            Ok(())
        })
    }

    fn discard_fork_agent_alias_reservations(
        &self,
        session_id: SessionId,
    ) -> AgentGraphStoreFuture<'_, bool> {
        Box::pin(async move {
            if self.fail_discard.swap(/*val*/ false, Ordering::AcqRel) {
                self.discard_failed.notify_one();
                return Err(AgentGraphStoreError::Internal {
                    message: "retry fork alias cleanup".to_string(),
                });
            }
            self.inner
                .discard_fork_agent_alias_reservations(session_id)
                .await
        })
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

#[derive(Clone, Copy)]
enum ForkOutcome {
    Publish,
    Ephemeral,
    Cancel,
    AmbiguousReservation,
    RetryCleanup,
}

#[test_case(ForkOutcome::Publish; "publish_after_reservation")]
#[test_case(ForkOutcome::Ephemeral; "ephemeral_fork_has_no_durable_reservations")]
#[test_case(ForkOutcome::Cancel; "cancel_during_reservation")]
#[test_case(ForkOutcome::AmbiguousReservation; "ambiguous_reservation")]
#[test_case(ForkOutcome::RetryCleanup; "retain_failed_cleanup")]
#[tokio::test]
async fn history_fork_owns_reservation_until_publication_or_cleanup(outcome: ForkOutcome) {
    let home = tempdir().expect("home");
    let mut config = test_config().await;
    config.codex_home = home.path().abs();
    config.cwd = config.codex_home.clone();
    config.features.enable(Feature::Sqlite).expect("SQLite");
    let state_db = init_state_db(&config).await;
    let graph = Arc::new(GatedForkGraph {
        inner: local_agent_graph_store_from_state_db(state_db.as_ref()).expect("graph"),
        reserved: Notify::new(),
        release: Notify::new(),
        discard_failed: Notify::new(),
        fail_reservation: matches!(outcome, ForkOutcome::AmbiguousReservation),
        fail_discard: AtomicBool::new(matches!(outcome, ForkOutcome::RetryCleanup)),
    });
    let store = Arc::new(InMemoryThreadStore::default());
    let mut manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("test"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        state_db,
    );
    {
        let state = Arc::get_mut(&mut manager.state).expect("unshared manager");
        state.thread_store = store.clone();
        state.agent_graph_store = Some(graph.clone());
    }
    let manager = Arc::new(manager);
    let source = manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            history_mode: Some(ThreadHistoryMode::Legacy),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("source");
    let inherited = graph
        .inner
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source.thread_id.into(),
            parent_thread_id: source.thread_id,
            child_thread_id: ThreadId::new(),
            nickname: Some("Historical".to_string()),
            task_path: None,
        })
        .await
        .expect("source alias");
    let fork_id = manager.reserve_thread_id();
    let source_id = source.thread_id;
    let shutdowns = store.calls().await.shutdown_thread;
    config.ephemeral = matches!(outcome, ForkOutcome::Ephemeral);
    let forking = Arc::clone(&manager);
    let fork = tokio::spawn(async move {
        forking
            .fork_thread_from_history(
                ForkSnapshot::Interrupted,
                StartThreadOptions {
                    environments: Some(Vec::new()),
                    history_mode: Some(ThreadHistoryMode::Legacy),
                    reserved_thread_id: Some(fork_id),
                    ..StartThreadOptions::new(config)
                },
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: source_id,
                    history: Arc::new(Vec::new()),
                    rollout_path: None,
                }),
            )
            .await
    });
    if matches!(outcome, ForkOutcome::Ephemeral) {
        let published = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), fork)
            .await
            .expect("ephemeral fork does not wait for durable reservation")
            .expect("fork worker")
            .expect("ephemeral fork");
        assert_eq!(
            graph
                .inner
                .list_agent_aliases(fork_id.into())
                .await
                .expect("aliases"),
            Vec::<AgentAlias>::new()
        );
        assert_eq!(
            graph
                .inner
                .list_agent_nickname_reservations(fork_id.into())
                .await
                .expect("reservations"),
            Vec::<String>::new()
        );
        assert!(manager.get_thread(fork_id).await.is_ok());
        published
            .thread
            .shutdown_durably_and_wait()
            .await
            .expect("stop fork");
        source
            .thread
            .shutdown_durably_and_wait()
            .await
            .expect("stop source");
        return;
    }
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), graph.reserved.notified())
        .await
        .expect("reservation reached");
    assert!(manager.get_thread(fork_id).await.is_err());
    assert!(
        manager
            .state
            .agent_lifecycle_lock(fork_id)
            .try_lock_owned()
            .is_err()
    );

    if matches!(outcome, ForkOutcome::Cancel | ForkOutcome::RetryCleanup) {
        fork.abort();
        match fork.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("caller must be cancelled"),
        }
        graph.release.notify_one();
    } else {
        graph.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), fork)
            .await
            .expect("fork finishes after reservation")
            .expect("fork worker");
        match outcome {
            ForkOutcome::Publish => {
                let published = result.expect("fork published");
                let aliases = graph
                    .inner
                    .list_agent_aliases(fork_id.into())
                    .await
                    .expect("aliases");
                assert_eq!(
                    aliases,
                    vec![AgentAlias {
                        session_id: fork_id.into(),
                        thread_id: fork_id,
                        agent_ref: 1,
                        nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
                        task_path: Some("/root".to_string()),
                        state: codex_agent_graph_store::AgentAliasState::Active,
                    }]
                );
                assert_eq!(
                    graph
                        .inner
                        .list_agent_nickname_reservations(fork_id.into())
                        .await
                        .expect("reservations"),
                    vec!["Historical".to_string()]
                );
                let allocated = graph
                    .inner
                    .allocate_agent_alias(AllocateAgentAliasRequest {
                        session_id: fork_id.into(),
                        parent_thread_id: fork_id,
                        child_thread_id: ThreadId::new(),
                        nickname: Some("Fresh".to_string()),
                        task_path: None,
                    })
                    .await
                    .expect("fresh alias");
                assert!(allocated.agent_ref > inherited.agent_ref);
                assert!(manager.get_thread(inherited.thread_id).await.is_err());
                published
                    .thread
                    .shutdown_durably_and_wait()
                    .await
                    .expect("stop fork");
            }
            ForkOutcome::AmbiguousReservation => {
                let error = match result {
                    Ok(_) => panic!("ambiguous reservation must not publish"),
                    Err(error) => error,
                };
                assert!(
                    error
                        .to_string()
                        .contains("reservation acknowledgement lost")
                );
            }
            ForkOutcome::Cancel | ForkOutcome::RetryCleanup | ForkOutcome::Ephemeral => {
                unreachable!()
            }
        }
    }
    if !matches!(outcome, ForkOutcome::Publish) {
        if matches!(outcome, ForkOutcome::RetryCleanup) {
            tokio::time::timeout(
                Duration::from_secs(/*secs*/ 10),
                graph.discard_failed.notified(),
            )
            .await
            .expect("first cleanup fails");
            assert!(
                manager
                    .state
                    .agent_lifecycle_lock(fork_id)
                    .try_lock_owned()
                    .is_err()
            );
        }
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            loop {
                if graph
                    .inner
                    .list_agent_aliases(fork_id.into())
                    .await
                    .expect("aliases")
                    .is_empty()
                    && manager
                        .state
                        .agent_lifecycle_lock(fork_id)
                        .try_lock_owned()
                        .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
        })
        .await
        .expect("durable cleanup releases namespace and lifecycle");
        assert!(manager.get_thread(fork_id).await.is_err());
        assert!(store.calls().await.shutdown_thread > shutdowns);
    }
    source
        .thread
        .shutdown_durably_and_wait()
        .await
        .expect("stop source");
}
