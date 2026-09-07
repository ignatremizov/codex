use super::*;
use crate::UserAgentFinalResponseHandling;
use crate::UserAgentReplyRouteMode;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;
use crate::agent::control::TargetMessageAdmission;
use crate::agent::control::TargetMessageAdmissionMode;
use crate::agent::control::TargetMessageRouteMode;

#[tokio::test]
async fn live_revert_rebinds_send_policy_without_turn_scoped_wake_grants() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let state_db = init_state_db(&config).await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        state_db,
    );
    let root = manager
        .start_thread(StartThreadOptions {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("start paginated root");
    let control = root.thread.session.services.agent_control.clone();
    let first = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("spawn first child");
    let second = root
        .thread
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::from_parts(
                /*commentary*/ true,
                UserAgentFinalResponseHandling::Wake,
                /*target_messages*/ true,
                /*queue_input*/ false,
            ),
            ..Default::default()
        })
        .await
        .expect("spawn second child");
    let ended = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("spawn endpoint that will close during replacement");
    let first_thread = manager
        .get_thread(first.target_thread_id)
        .await
        .expect("first child");
    let second_thread = manager
        .get_thread(second.target_thread_id)
        .await
        .expect("second child");
    root.thread
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await
        .expect("enable root subtree");
    first_thread
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
        .await
        .expect("nested disable");
    root.thread
        .set_agent_reply_route(
            &first.target_thread_id.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Disabled,
        )
        .await
        .expect("explicit upward disable");
    root.thread
        .set_agent_reply_route(
            &second.target_thread_id.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await
        .expect("explicit upward enable");
    root.thread
        .set_agent_reply_route(
            &ended.target_thread_id.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await
        .expect("enable endpoint before closing it");
    let ended_presentation = manager
        .get_thread(ended.target_thread_id)
        .await
        .expect("endpoint before close")
        .session
        .presentation_id();
    let old_root = root.thread.session.presentation_id();
    let sender = second_thread.session.presentation_id();
    let TargetMessageAdmission::Wake(wake) = control
        .target_message_admission(
            old_root,
            sender,
            "old-sender-turn",
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            TargetMessageAdmissionMode::SteerOrWake,
        )
        .expect("reserve old live-route wake")
    else {
        panic!("expected an idle wake reservation");
    };
    let snapshot = root
        .thread
        .snapshot_agent_messaging_for_revert()
        .await
        .expect("capture live policy")
        .expect("V1 snapshot");
    let ended_thread = manager
        .get_thread(ended.target_thread_id)
        .await
        .expect("endpoint before authoritative close");
    let ended_snapshot = ended_thread
        .snapshot_agent_messaging_for_revert()
        .await
        .expect("mark endpoint before close");
    root.thread
        .close_agent(
            &ended.target_thread_id.to_string(),
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect("separate close ends that endpoint's grants");
    drop(ended_snapshot);
    assert_eq!(
        control.target_message_route_mode(old_root, ended_presentation),
        None
    );
    root.thread.ensure_rollout_materialized().await;
    root.thread.flush_rollout().await.expect("flush root");
    let rollout = root.thread.rollout_path().expect("root rollout");
    root.thread
        .shutdown_and_wait()
        .await
        .expect("shutdown old runtime");
    manager
        .remove_thread(&root.thread_id)
        .await
        .expect("remove old runtime");
    let history = manager
        .initial_history_from_rollout_path(rollout)
        .await
        .expect("load history");
    let replacement = manager
        .resume_thread_after_live_revert(
            config.clone(),
            history,
            Arc::clone(&manager.state.auth_manager),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
            Some(snapshot),
        )
        .await
        .expect("replace live root");
    let current_root = replacement.thread.session.presentation_id();
    assert_ne!(old_root, current_root);
    assert_eq!(
        (
            control.target_message_route_mode(current_root, first_thread.session.presentation_id()),
            control.target_message_route_mode(current_root, sender),
            control.target_message_route_mode(first_thread.session.presentation_id(), sender),
        ),
        (
            Some(TargetMessageRouteMode::Disabled),
            Some(TargetMessageRouteMode::Enabled),
            Some(TargetMessageRouteMode::Enabled),
        )
    );
    assert!(!control.target_message_wake_is_current(current_root, sender, "old-sender-turn", wake));
    assert!(!control.target_message_binding_pending(current_root, sender));
    assert!(!control.has_completion_watcher(current_root, sender));
    assert_eq!(
        control.target_message_route_mode(current_root, ended_presentation),
        None
    );

    // Replacing a nested supervisor preserves its own disable over Main's surviving enable.
    let nested_sender = first_thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("spawn nested sender");
    let nested_receiver = first_thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("spawn nested receiver");
    let nested_sender = manager
        .get_thread(nested_sender.target_thread_id)
        .await
        .expect("nested sender runtime");
    let nested_receiver = manager
        .get_thread(nested_receiver.target_thread_id)
        .await
        .expect("nested receiver runtime");
    let nested_snapshot = first_thread
        .snapshot_agent_messaging_for_revert()
        .await
        .expect("capture nested policy")
        .expect("V1 snapshot");
    first_thread.ensure_rollout_materialized().await;
    first_thread
        .flush_rollout()
        .await
        .expect("flush nested supervisor");
    let nested_rollout = first_thread.rollout_path().expect("nested rollout");
    first_thread
        .shutdown_and_wait()
        .await
        .expect("shutdown nested runtime");
    manager
        .remove_thread(&first.target_thread_id)
        .await
        .expect("remove nested runtime");
    // Deliberately pause at the absent-runtime boundary and force the same reconciliation
    // used by peer admission. No timers or scheduler assumptions are involved.
    control
        .refresh_subtree_messaging(nested_sender.session.presentation_id().thread_id)
        .await
        .expect("refresh while nested supervisor is absent");
    assert_eq!(
        control.target_message_route_mode(
            nested_receiver.session.presentation_id(),
            nested_sender.session.presentation_id(),
        ),
        Some(TargetMessageRouteMode::Disabled),
    );
    assert!(
        control
            .target_message_admission(
                nested_receiver.session.presentation_id(),
                nested_sender.session.presentation_id(),
                "paused-revert-peer-turn",
                Some("receiver-turn"),
                /*observer_last_terminal_turn_id*/ None,
                TargetMessageAdmissionMode::SteerOrWake,
            )
            .is_err()
    );
    let nested_history = manager
        .initial_history_from_rollout_path(nested_rollout)
        .await
        .expect("load nested history");
    let nested = manager
        .resume_thread_after_live_revert(
            config.clone(),
            nested_history,
            Arc::clone(&manager.state.auth_manager),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
            Some(nested_snapshot),
        )
        .await
        .expect("replace nested runtime");
    assert_eq!(
        nested
            .thread
            .set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
            .await
            .expect("read existing nested override"),
        Some(UserAgentReplyRouteMode::Disabled)
    );
    // Cancelling capture before shutdown leaves the current runtime's policy intact.
    drop(
        nested
            .thread
            .snapshot_agent_messaging_for_revert()
            .await
            .expect("capture cancellable live policy"),
    );
    assert_eq!(
        nested
            .thread
            .set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
            .await
            .expect("read policy after cancellation before shutdown"),
        Some(UserAgentReplyRouteMode::Disabled),
    );
    // Capture published A, then let the actual live-revert handoff rekey policy
    // to pending B before the stale refresh is allowed to validate/prune.
    let old_nested = nested.thread.session.presentation_id();
    let rekey_snapshot = nested
        .thread
        .snapshot_agent_messaging_for_revert()
        .await
        .expect("capture nested policy for gated rekey")
        .expect("V1 snapshot");
    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
    *manager
        .state
        .wait_agent_presentations
        .messaging_refresh_capture_gate
        .lock()
        .expect("capture gate") = Some((captured_tx, proceed_rx));
    let refresh_control = control.clone();
    let refresh_id = nested_sender.session.presentation_id().thread_id;
    let stale_refresh =
        tokio::spawn(async move { refresh_control.refresh_subtree_messaging(refresh_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), captured_rx)
        .await
        .expect("refresh reaches runtime capture")
        .expect("capture notification");
    nested.thread.ensure_rollout_materialized().await;
    nested
        .thread
        .flush_rollout()
        .await
        .expect("flush before gated rekey");
    let rollout = nested.thread.rollout_path().expect("nested rollout");
    nested
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown captured A");
    manager
        .remove_thread(&first.target_thread_id)
        .await
        .expect("remove captured A");
    let history = manager
        .initial_history_from_rollout_path(rollout)
        .await
        .expect("load gated replacement history");
    let (attempted_tx, mut attempted_rx) = tokio::sync::mpsc::unbounded_channel();
    *manager
        .state
        .wait_agent_presentations
        .messaging_refresh_attempted
        .lock()
        .expect("refresh attempts") = Some(attempted_tx);
    let resume = manager.resume_thread_after_live_revert(
        config,
        history,
        Arc::clone(&manager.state.auth_manager),
        /*parent_trace*/ None,
        ClientMcpExtensions::default(),
        Some(rekey_snapshot),
    );
    let (nested, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(resume, async {
            loop {
                let markers = attempted_rx
                    .recv()
                    .await
                    .expect("replacement refresh attempt");
                if markers
                    .iter()
                    .any(|source| source.thread_id == old_nested.thread_id && *source != old_nested)
                {
                    break;
                }
            }
            // B is rekeyed and waiting for the mutex held by the A snapshot.
            proceed_tx.send(()).expect("release stale runtime capture");
            stale_refresh
                .await
                .expect("refresh task")
                .expect("retry stale refresh");
        })
    })
    .await
    .expect("rekey and stale refresh complete without a lifecycle fence");
    let nested = nested.expect("publish replacement after stale capture retry");
    let _ = manager
        .state
        .wait_agent_presentations
        .messaging_refresh_attempted
        .lock()
        .expect("refresh attempts")
        .take();
    assert_ne!(nested.thread.session.presentation_id(), old_nested);
    assert_eq!(
        control.target_message_route_mode(
            nested_receiver.session.presentation_id(),
            nested_sender.session.presentation_id(),
        ),
        Some(TargetMessageRouteMode::Disabled),
        "stale capture must not delete B's nested disable and expose the root enable",
    );
    replacement
        .thread
        .set_agent_reply_route(
            &second.target_thread_id.to_string(),
            Some(&first.target_thread_id.to_string()),
            UserAgentReplyRouteMode::Enabled,
        )
        .await
        .expect("incident explicit policy for cancellation");
    let cancelled = nested
        .thread
        .snapshot_agent_messaging_for_revert()
        .await
        .expect("capture policy before failed reload");
    let cancelled_presentation = nested.thread.session.presentation_id();
    nested
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown before cancellation");
    manager
        .remove_thread(&first.target_thread_id)
        .await
        .expect("remove before cancellation");
    drop(cancelled);
    // Cleanup must be synchronous: do not refresh before checking the incident route.
    assert_eq!(
        control.target_message_route_mode(cancelled_presentation, sender),
        None
    );
    replacement
        .thread
        .close_agent(
            &first.target_thread_id.to_string(),
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect("close first child");
    replacement
        .thread
        .close_agent(
            &second.target_thread_id.to_string(),
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect("close second child");
    replacement
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown replacement");
}
