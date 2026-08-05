use crate::StartThreadOptions;
use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::agent::control::InitialTerminalObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::ThreadManagerState;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn residency_slot_reservation_unloads_oldest_idle_v2_agent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict the first idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second = spawn_v2_subagent(&control, &state, config, root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
}

#[tokio::test]
async fn residency_does_not_evict_an_agent_with_an_owned_lifecycle_boundary() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;
    let lifecycle_guard = state
        .agent_lifecycle_lock(first.thread_id)
        .lock_owned()
        .await;

    let reservation = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await;
    let Err(err) = reservation else {
        panic!("busy lifecycle boundary should prevent eviction");
    };
    assert_matches::assert_matches!(err.details(), CodexErrorDetails::AgentLimitReached { .. });
    let still_loaded = manager
        .get_thread(first.thread_id)
        .await
        .expect("busy resident should remain loaded");
    assert!(Arc::ptr_eq(&still_loaded, &first.thread));

    drop(lifecycle_guard);
    let replacement_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("released resident should become evictable");
    drop(replacement_slot);
    let Err(err) = manager.get_thread(first.thread_id).await else {
        panic!("released resident should be evicted");
    };
    assert_matches::assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(thread_id) if *thread_id == first.thread_id
    );
}

#[tokio::test]
async fn interrupted_v2_agent_reloads_after_residency_eviction() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_interrupted(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict the first interrupted idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;

    control
        .ensure_v2_agent_loaded(config, first.thread_id)
        .await
        .expect("evicted interrupted agent should reload from its persisted rollout");

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(first.thread_id).await.is_ok());
    match manager.get_thread(second.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => {
                assert_eq!(*thread_id, second.thread_id)
            }
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
}

#[tokio::test]
async fn interrupted_v2_residency_eviction_does_not_notify_parent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let child_path = AgentPath::root().join("worker_1").expect("child path");
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });
    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = state
        .spawn_new_thread_with_source(
            config.clone(),
            control.clone(),
            child_source.clone(),
            /*history_mode*/ None,
            Some(root.thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("spawn v2 child");
    control
        .maybe_start_completion_watcher(
            &first.thread,
            Some(child_source),
            child_path.to_string(),
            Some(child_path),
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    first_slot.commit(first.thread_id);
    mark_thread_interrupted(first.thread.as_ref()).await;

    let _second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict interrupted child");

    let unexpected_notification =
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                let event = root.thread.next_event().await.expect("root event");
                if let EventMsg::ItemCompleted(event) = event.msg
                    && let codex_protocol::items::TurnItem::AgentMessage(item) = event.item
                    && item.has_sub_agent_completion_identity()
                {
                    break;
                }
            }
        })
        .await;
    assert!(unexpected_notification.is_err());
}

#[tokio::test]
async fn retained_v2_eviction_rebinds_foreign_v1_watcher_after_reload() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let child_owner = manager.agent_control();
    let foreign_observer = manager.agent_control();
    let state = child_owner
        .upgrade()
        .expect("thread manager should be live");
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(AgentPath::root().join("worker_1").expect("child path")),
        agent_nickname: None,
        agent_role: None,
    });
    let first_slot = child_owner
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = state
        .spawn_new_thread_with_source(
            config.clone(),
            child_owner.clone(),
            child_source.clone(),
            /*history_mode*/ None,
            Some(root.thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("spawn independently controlled V2 child");
    first_slot.commit(first.thread_id);
    let retained_first = Arc::clone(&first.thread);
    let root_presentation = root.thread.session.presentation_id();
    let first_presentation = first.thread.session.presentation_id();
    foreign_observer
        .ensure_v1_completion_watcher(
            first.thread_id,
            child_source,
            ResponseObservationPolicy::default(),
            first.thread.agent_status().await,
        )
        .await
        .expect("foreign V1 watcher should attach");
    assert!(
        !Arc::ptr_eq(
            &child_owner.wait_agent_presentations,
            &foreign_observer.wait_agent_presentations,
        ),
        "test requires distinct owner and observer presentation registries"
    );
    mark_thread_completed(first.thread.as_ref()).await;
    assert!(foreign_observer.has_completion_watcher(root_presentation, first_presentation));

    let pending_slot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match child_owner
                .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
                .await
            {
                Ok(slot) => break slot,
                Err(err)
                    if matches!(err.details(), CodexErrorDetails::AgentLimitReached { .. }) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(err) => panic!("second slot reservation failed unexpectedly: {err}"),
            }
        }
    })
    .await
    .expect("completed child's transient watcher work should release its lifecycle boundary");
    drop(pending_slot);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while foreign_observer.has_completion_watcher(root_presentation, first_presentation) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("foreign watcher should leave the retained runtime");
    assert_eq!(
        retained_first.agent_status().await,
        AgentStatus::Completed(Some("done".to_string()))
    );

    child_owner
        .ensure_v2_agent_loaded(config, first.thread_id)
        .await
        .expect("evicted child should reload");
    let reloaded = manager
        .get_thread(first.thread_id)
        .await
        .expect("reloaded child should be live");
    let reloaded_presentation = reloaded.session.presentation_id();
    assert_ne!(reloaded_presentation, first_presentation);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !foreign_observer.has_completion_watcher(root_presentation, reloaded_presentation) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("foreign watcher should rebind to the reloaded runtime");
    assert!(
        foreign_observer
            .response_observation_snapshots(root_presentation, first_presentation)
            .is_empty(),
        "rebind should remove observation state for the evicted presentation"
    );
    assert!(
        !foreign_observer
            .response_observation_snapshots(root_presentation, reloaded_presentation)
            .is_empty(),
        "rebind should retain observation state on the reloaded presentation"
    );
}

async fn spawn_v2_subagent(
    control: &AgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    label: &str,
) -> crate::thread_manager::NewThread {
    state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::Other(label.to_string())),
            /*history_mode*/ None,
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("spawn v2 subagent")
}

async fn mark_thread_completed(thread: &CodexThread) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn mark_thread_interrupted(thread: &CodexThread) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn.sub_id.clone()),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn clear_active_turn(thread: &CodexThread) {
    // The fixture has no task runner to clear the turn after the terminal event.
    *thread.session.active_turn.lock().await = None;
}
