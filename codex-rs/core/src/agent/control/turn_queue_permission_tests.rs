use super::*;
use crate::ThreadManager;
use crate::UserAgentReplyRouteMode;
use crate::UserAgentSpawnOptions;
use crate::config::test_config;
use crate::init_state_db;
use crate::thread_manager::StartThreadOptions;
use codex_login::CodexAuth;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum Revoke {
    Directed,
    Subtree,
    ExpiredBeforeEligibility,
    ExpiredAfterEligibility,
}

#[test_case(Revoke::Directed; "directed_disable_wins_after_early_check")]
#[test_case(Revoke::Subtree; "subtree_disable_wins_after_early_check")]
#[test_case(Revoke::ExpiredBeforeEligibility; "expired_entry_warns_before_waits")]
#[test_case(Revoke::ExpiredAfterEligibility; "expired_entry_warns_at_exact_admission")]
#[tokio::test]
async fn revoked_queued_message_is_not_admitted_and_warns_without_waking_source(revoke: Revoke) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
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
        .start_thread(StartThreadOptions::new(config))
        .await
        .expect("root");
    let control = root.thread.session.services.agent_control.clone();
    let sender = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("sender");
    let receiver = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("receiver");
    let sender = manager
        .get_thread(sender.target_thread_id)
        .await
        .expect("sender runtime");
    let receiver = manager
        .get_thread(receiver.target_thread_id)
        .await
        .expect("receiver runtime");
    root.thread
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await
        .expect("enable sibling sends");
    let source = sender.session.presentation_id();
    let target = receiver.session.presentation_id();
    let TargetMessageAdmission::Wake(reservation_id) = control
        .target_message_admission(
            target,
            source,
            "queued-source-turn",
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            TargetMessageAdmissionMode::SeparateTurn,
        )
        .expect("reserve queued wake")
    else {
        panic!("expected wake");
    };
    let queue_id = uuid::Uuid::now_v7();
    let source_status = sender.agent_status().await;
    let target_status = receiver.agent_status().await;
    let captured_before = manager.captured_ops().len();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
    if matches!(revoke, Revoke::ExpiredBeforeEligibility) {
        control.rollback_target_message_wake_reservation(
            target,
            source,
            "queued-source-turn",
            reservation_id,
        );
    } else {
        *control
            .wait_agent_presentations
            .scoped_permission_check_gate
            .lock()
            .expect("permission gate") = Some((reached_tx, proceed_rx));
    }
    let state = control.upgrade().expect("manager");
    AgentControl::enqueue_agent_turn(
        &state,
        QueuedAgentTurn {
            id: queue_id,
            control: control.clone(),
            source,
            target_thread_id: target.thread_id,
            input: AgentControlInput::User(vec![UserInput::Text {
                text: "must never reach exact admission".to_string(),
                text_elements: Vec::new(),
            }]),
            start_options: TurnStartOptions::default(),
            response_observation: ResponseObservationPolicy::from_turn_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
                /*target_messages*/ false,
                /*queue_input*/ true,
            ),
            task_preview: None,
            authored_selector: None,
            target_message_wake: Some(QueuedTargetMessageWake {
                observer: target,
                target: source,
                target_turn_id: "queued-source-turn".to_string(),
                reservation_id,
            }),
        },
    );
    if !matches!(revoke, Revoke::ExpiredBeforeEligibility) {
        tokio::time::timeout(std::time::Duration::from_secs(10), reached_rx)
            .await
            .expect("worker reaches post-capacity gate")
            .expect("gate signal");
        // The worker already passed its eligibility check and holds B's lifecycle.
        // Both changes must commit while it is paused, without acquiring A/B lifecycles
        // in the opposite order or waiting for the queue's capacity retry.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            match revoke {
                Revoke::Directed => {
                    control
                        .replace_durable_target_message_route(
                            source.thread_id,
                            target.thread_id,
                            TargetMessageRouteMode::Disabled,
                        )
                        .await
                        .expect("directed disable");
                }
                Revoke::Subtree => {
                    root.thread
                        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
                        .await
                        .expect("subtree disable");
                }
                Revoke::ExpiredAfterEligibility => {
                    control.rollback_target_message_wake_reservation(
                        target,
                        source,
                        "queued-source-turn",
                        reservation_id,
                    );
                }
                Revoke::ExpiredBeforeEligibility => unreachable!(),
            }
        })
        .await
        .expect("revocation is not blocked by paused admission");
        proceed_tx.send(()).expect("resume exact admission");
    }
    let warning = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = sender.next_event().await.expect("source event");
            if event.id.starts_with(&format!("agent-queue-{queue_id}-")) {
                break event;
            }
        }
    })
    .await
    .expect("source receives queue warning");
    let early = matches!(revoke, Revoke::ExpiredBeforeEligibility);
    let reason = if early {
        "message permission was revoked or its scoped wake expired".to_string()
    } else {
        CodexErr::InvalidRequest(
            "queued message permission was revoked or its scoped wake expired".to_string(),
        )
        .to_string()
    };
    let warning_kind = if early { "permission" } else { "admission" };
    assert_eq!(
        serde_json::to_value(warning).expect("serialize warning"),
        serde_json::to_value(Event {
            id: format!("agent-queue-{queue_id}-{warning_kind}"),
            msg: EventMsg::Warning(WarningEvent {
                message: format!(
                    "Queued input {queue_id} for {} was discarded before admission: {reason}",
                    target.thread_id,
                ),
            }),
        })
        .expect("serialize expected warning")
    );
    assert_eq!(
        manager.captured_ops().len(),
        captured_before,
        "no target input was submitted"
    );
    assert_eq!(
        (sender.agent_status().await, receiver.agent_status().await),
        (source_status, target_status)
    );
    assert!(!control.target_message_wake_is_current(
        target,
        source,
        "queued-source-turn",
        reservation_id,
    ));
    sender.shutdown_and_wait().await.expect("shutdown sender");
    receiver
        .shutdown_and_wait()
        .await
        .expect("shutdown receiver");
    root.thread
        .shutdown_and_wait()
        .await
        .expect("shutdown root");
}
