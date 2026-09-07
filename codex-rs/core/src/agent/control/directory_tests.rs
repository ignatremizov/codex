use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

async fn directory_harness() -> AgentControlHarness {
    let (home, mut config) = test_config().await;
    config.list_agents_enabled = true;
    AgentControlHarness::new_with_config(home, config).await
}

async fn directory_child(
    harness: &AgentControlHarness,
    control: &AgentControl,
    parent_thread_id: ThreadId,
    task: &str,
) -> (ThreadId, Arc<CodexThread>) {
    let child = control
        .spawn_idle_agent_with_metadata(
            harness.config.clone(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                task: Some(task.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("spawn idle directory member");
    let thread = harness
        .manager
        .get_thread(child.thread_id)
        .await
        .expect("published child runtime");
    (child.thread_id, thread)
}

#[tokio::test]
async fn directory_filters_before_pagination_and_never_serializes_completion_payloads() {
    let harness = directory_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    let (first_id, first) = directory_child(&harness, control, root_id, "backend/a").await;
    let (closed_id, _) = directory_child(&harness, control, root_id, "backend/closed").await;
    let (last_id, last) = directory_child(&harness, control, root_id, "backend/b").await;
    for thread in [&first, &last] {
        thread
            .session
            .send_event_raw(Event {
                id: "directory-complete".to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "directory-complete".to_string(),
                    last_agent_message: Some("PRIVATE FULL RESPONSE".to_string()),
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                }),
            })
            .await;
    }
    control
        .close_agent(closed_id)
        .await
        .expect("close middle member");
    let mut expected = Vec::new();
    for (agent_id, task_path) in [(first_id, "/root/backend/a"), (last_id, "/root/backend/b")] {
        let alias = control
            .find_session_agent_alias(agent_id)
            .await
            .expect("alias query")
            .expect("owned alias");
        expected.push(AgentDirectoryEntry {
            agent_id,
            agent_ref: alias.agent_ref.to_string(),
            nickname: alias.nickname,
            role: Some("worker".to_string()),
            task_path: Some(task_path.to_string()),
            status: AgentDirectoryEntryStatus::Completed,
        });
    }
    let second = expected.pop().expect("second entry");
    let first = expected.pop().expect("first entry");
    let cursor = first.agent_ref.clone();
    let page = control
        .list_agent_directory(
            root_id,
            Some("backend"),
            AgentDirectoryStatus::Completed,
            /*cursor*/ None,
            /*limit*/ Some(1),
        )
        .await
        .expect("first filtered page");
    assert_eq!(
        page,
        AgentDirectoryPage {
            agents: vec![first],
            next_cursor: Some(cursor.clone()),
        }
    );
    let page = control
        .list_agent_directory(
            root_id,
            Some("backend"),
            AgentDirectoryStatus::Completed,
            Some(&cursor),
            /*limit*/ Some(1),
        )
        .await
        .expect("second filtered page");
    assert_eq!(
        page,
        AgentDirectoryPage {
            agents: vec![second],
            next_cursor: None,
        }
    );
    let loaded = control
        .list_agent_directory(
            root_id,
            Some("backend"),
            AgentDirectoryStatus::Loaded,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("loaded includes completed members");
    assert_eq!(
        loaded
            .agents
            .iter()
            .map(|entry| entry.agent_id)
            .collect::<Vec<_>>(),
        vec![first_id, last_id]
    );
    assert!(
        !serde_json::to_string(&loaded)
            .expect("serialize page")
            .contains("PRIVATE")
    );
    let closed = control
        .list_agent_directory(
            root_id,
            Some("backend"),
            AgentDirectoryStatus::Closed,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("closed member remains discoverable explicitly");
    assert_eq!(
        closed
            .agents
            .iter()
            .map(|entry| (&entry.task_path, entry.status))
            .collect::<Vec<_>>(),
        vec![(
            &Some("/root/backend/closed".to_string()),
            AgentDirectoryEntryStatus::Closed
        )]
    );
    assert_eq!(
        control
            .find_session_agent_alias(closed_id)
            .await
            .expect("closed alias query")
            .expect("closed alias retained")
            .task_path,
        Some("/root/backend/closed".to_string())
    );
    harness.manager.remove_thread(&root_id).await;
    assert!(
        !control
            .agent_directory_enabled(last_id)
            .await
            .expect("absent root denies")
    );
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn directory_uses_caller_relative_prefixes_but_unfiltered_scope_is_the_owned_root() {
    let harness = directory_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    let (backend_id, backend) = directory_child(&harness, control, root_id, "backend").await;
    let (auth_id, _) = directory_child(&harness, control, backend_id, "auth").await;
    let (frontend_id, _) = directory_child(&harness, control, root_id, "frontend").await;
    let (foreign_root_id, foreign_root) = harness.start_thread().await;
    let foreign_control = &foreign_root.session.services.agent_control;
    let (foreign_id, _) =
        directory_child(&harness, foreign_control, foreign_root_id, "backend/auth").await;
    let child_control = &backend.session.services.agent_control;
    let relative = child_control
        .list_agent_directory(
            backend_id,
            Some("auth"),
            AgentDirectoryStatus::All,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("relative child prefix");
    assert_eq!(
        relative
            .agents
            .iter()
            .map(|entry| entry.agent_id)
            .collect::<Vec<_>>(),
        vec![auth_id]
    );
    let unfiltered = child_control
        .list_agent_directory(
            backend_id,
            /*path_prefix*/ None,
            AgentDirectoryStatus::All,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("whole owned graph from child");
    assert_eq!(
        unfiltered
            .agents
            .iter()
            .map(|entry| entry.agent_id)
            .collect::<Vec<_>>(),
        vec![root_id, backend_id, auth_id, frontend_id]
    );
    let absolute = child_control
        .list_agent_directory(
            backend_id,
            Some("/root"),
            AgentDirectoryStatus::All,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("/root is a valid directory prefix");
    assert_eq!(absolute, unfiltered);
    assert!(
        control
            .list_agent_directory(
                foreign_id,
                /*path_prefix*/ None,
                AgentDirectoryStatus::All,
                /*cursor*/ None,
                /*limit*/ None,
            )
            .await
            .is_err()
    );
    root.shutdown_and_wait().await.expect("shutdown root");
    foreign_root
        .shutdown_and_wait()
        .await
        .expect("shutdown foreign root");
}

#[tokio::test]
async fn directory_gate_uses_root_config_and_rejects_disabled_queries() {
    let mut harness = AgentControlHarness::new().await;
    let (root_id, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    assert!(
        !control
            .agent_directory_enabled(root_id)
            .await
            .expect("root gate")
    );
    let error = control
        .list_agent_directory(
            root_id,
            Some("/invalid"),
            AgentDirectoryStatus::All,
            Some("invalid"),
            /*limit*/ Some(0),
        )
        .await
        .expect_err("deny before parsing or enumerating");
    assert!(
        matches!(error.details, crate::error::CodexErrorDetails::InvalidRequest(message) if message.contains("disabled"))
    );
    harness.config.list_agents_enabled = true;
    let (child_id, child) = directory_child(&harness, control, root_id, "worker").await;
    assert!(
        !child
            .session
            .services
            .agent_control
            .agent_directory_enabled(child_id)
            .await
            .expect("child cannot enable its root directory")
    );
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn directory_absent_root_does_not_initialize_a_durable_namespace() {
    let harness = directory_harness().await;
    let root_id = ThreadId::new();
    let control = harness
        .control
        .clone()
        .with_session_id(SessionId::from(root_id), /*max_threads*/ 4);
    assert!(
        !control
            .agent_directory_enabled(root_id)
            .await
            .expect("absent root denies")
    );
    assert_eq!(
        harness
            .state_db
            .as_ref()
            .expect("state db")
            .find_agent_alias_by_thread(control.session_id(), root_id)
            .await
            .expect("query without initializing namespace"),
        None
    );
}

#[tokio::test]
async fn directory_running_interrupted_and_error_filters_follow_live_status_without_payloads() {
    let harness = directory_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    let (child_id, child) = directory_child(&harness, control, root_id, "worker").await;
    let scenarios = [
        (
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "directory-status".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
                agent_queue: None,
            }),
            AgentDirectoryStatus::Running,
            AgentDirectoryEntryStatus::Running,
        ),
        (
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("directory-status".to_string()),
                reason: TurnAbortReason::Interrupted,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }),
            AgentDirectoryStatus::Interrupted,
            AgentDirectoryEntryStatus::Interrupted,
        ),
        (
            EventMsg::Error(ErrorEvent {
                message: "PRIVATE ERROR DETAILS".to_string(),
                codex_error_info: None,
                misalignment: None,
            }),
            AgentDirectoryStatus::Errored,
            AgentDirectoryEntryStatus::Errored,
        ),
    ];
    for (msg, filter, status) in scenarios {
        child
            .session
            .send_event_raw(Event {
                id: "directory-status".to_string(),
                msg,
            })
            .await;
        let page = control
            .list_agent_directory(
                root_id,
                Some("worker"),
                filter,
                /*cursor*/ None,
                /*limit*/ Some(1),
            )
            .await
            .expect("specific live status");
        assert_eq!(
            page.agents
                .iter()
                .map(|entry| (entry.agent_id, entry.status))
                .collect::<Vec<_>>(),
            vec![(child_id, status)]
        );
        assert!(
            !serde_json::to_string(&page)
                .expect("serialize page")
                .contains("PRIVATE")
        );
    }
    for (cursor, limit) in [(Some("invalid"), Some(1)), (None, Some(0))] {
        assert!(
            control
                .list_agent_directory(
                    root_id,
                    /*path_prefix*/ None,
                    AgentDirectoryStatus::Loaded,
                    cursor,
                    limit,
                )
                .await
                .is_err()
        );
    }
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn directory_turn_projection_uses_root_setting_instead_of_child_config() {
    for enabled in [false, true] {
        let (home, mut config) = test_config().await;
        config.list_agents_enabled = enabled;
        let mut harness = AgentControlHarness::new_with_config(home, config).await;
        let (root_id, root) = harness.start_thread().await;
        let control = &root.session.services.agent_control;
        harness.config.list_agents_enabled = !enabled;
        let (child_id, child) = directory_child(&harness, control, root_id, "worker").await;
        let turn = child.session.new_default_turn().await;
        assert_eq!(
            (
                child.session.get_config().await.list_agents_enabled,
                turn.config.list_agents_enabled,
                control
                    .agent_directory_enabled(child_id)
                    .await
                    .expect("root gate"),
            ),
            (!enabled, enabled, enabled)
        );
        harness.manager.remove_thread(&root_id).await;
        let turn = child.session.new_default_turn().await;
        assert!(!turn.config.list_agents_enabled);
        assert!(harness.manager.get_thread(root_id).await.is_err());
        root.shutdown_and_wait().await.expect("shutdown root");
    }
}

#[tokio::test]
async fn directory_does_not_treat_unpublished_runtime_as_loaded() {
    let harness = directory_harness().await;
    let (root_id, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    let state = control.upgrade().expect("live manager");
    let pending = state
        .spawn_new_thread_with_source(
            harness.config.clone(),
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            }),
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(root_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            ThreadRuntimePublication::Deferred,
        )
        .await
        .expect("unpublished runtime");
    state
        .agent_graph_store()
        .expect("graph store")
        .allocate_agent_alias(codex_agent_graph_store::AllocateAgentAliasRequest {
            session_id: control.session_id(),
            parent_thread_id: root_id,
            child_thread_id: pending.thread_id,
            nickname: Some("Pending".to_string()),
            task_path: Some("/root/pending".to_string()),
        })
        .await
        .expect("durable alias during setup");
    let page = control
        .list_agent_directory(
            root_id,
            Some("pending"),
            AgentDirectoryStatus::Loaded,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("read only published loaded members");
    assert_eq!(
        serde_json::to_value(page).expect("serialize page"),
        json!({"agents": [], "next_cursor": null})
    );
    let page = control
        .list_agent_directory(
            root_id,
            Some("pending"),
            AgentDirectoryStatus::All,
            /*cursor*/ None,
            /*limit*/ None,
        )
        .await
        .expect("all includes the committed durable alias");
    assert_eq!(
        page.agents
            .iter()
            .map(|entry| (entry.agent_id, entry.status))
            .collect::<Vec<_>>(),
        vec![(pending.thread_id, AgentDirectoryEntryStatus::Unloaded)]
    );
    assert!(harness.manager.get_thread(pending.thread_id).await.is_err());
    control
        .discard_unpublished_agent_instance(&pending.thread, LiveAgentMetadataDisposition::Release)
        .await
        .expect("discard pending runtime");
    pending
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown pending");
    root.shutdown_and_wait().await.expect("shutdown root");
}
