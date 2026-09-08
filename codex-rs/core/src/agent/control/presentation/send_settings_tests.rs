#![expect(
    clippy::await_holding_invalid_type,
    reason = "permission checks deliberately run under the caller-owned messaging transaction"
)]

use super::*;
use crate::ThreadManager;
use crate::config::test_config;
use crate::init_state_db;
use crate::thread_manager::StartThreadOptions;
use codex_login::CodexAuth;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mailbox_permission_reads_closed_sender_settings_without_mutating_runtime() {
    let home = tempfile::tempdir().expect("home");
    let mut config = test_config().await;
    config.codex_home = home.path().abs();
    config.cwd = config.codex_home.abs();
    let state_db = init_state_db(&config).await.expect("state database");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(Arc::clone(&state_db)),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .expect("root");
    let control = root.thread.session.services.agent_control.clone();
    let sender = ThreadId::new();
    state_db
        .upsert_thread_spawn_edge(
            root.thread_id,
            sender,
            codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("closed sender ancestry");
    let original_status = root.thread.agent_status().await;
    let original_history: Vec<_> = root
        .thread
        .session
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect();
    let permission = control.acquire_messaging_permission_transaction().await;
    assert!(
        !control
            .mailbox_send_permission_locked(sender, root.thread_id)
            .await
            .expect("absent")
    );
    state_db
        .replace_agent_send_setting(
            AgentSendScope::Subtree {
                supervisor_thread_id: root.thread_id,
            },
            AgentSendMode::Enabled,
        )
        .await
        .expect("enable subtree");
    assert!(
        control
            .mailbox_send_permission_locked(sender, root.thread_id)
            .await
            .expect("inherited")
    );
    state_db
        .replace_agent_send_setting(
            AgentSendScope::Directed {
                sender_thread_id: sender,
                receiver_thread_id: root.thread_id,
            },
            AgentSendMode::Disabled,
        )
        .await
        .expect("explicit disable");
    assert!(
        !control
            .mailbox_send_permission_locked(sender, root.thread_id)
            .await
            .expect("disabled")
    );
    assert!(
        control
            .mailbox_send_permission_locked(root.thread_id, sender)
            .await
            .expect("downward")
    );
    drop(permission);
    control
        .publish_mailbox_rejection(sender, root.thread_id, "unloaded-message", "revoked")
        .await;
    control
        .publish_mailbox_rejection(root.thread_id, sender, "loaded-message", "revoked")
        .await;
    let warning = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = root.thread.next_event().await.expect("warning event");
            if event.id == "agent-mailbox-rejected-loaded-message" {
                break event.msg;
            }
        }
    })
    .await
    .expect("warning deadline");
    assert_eq!(
        warning,
        codex_protocol::protocol::EventMsg::Warning(codex_protocol::protocol::WarningEvent {
            message: format!("Mailbox message loaded-message to {sender} was rejected: revoked"),
        })
    );
    assert!(manager.get_thread(sender).await.is_err());
    assert_eq!(root.thread.agent_status().await, original_status);
    assert_eq!(
        root.thread
            .session
            .clone_history()
            .await
            .raw_items()
            .cloned()
            .collect::<Vec<_>>(),
        original_history
    );
    assert!(
        control
            .wait_agent_presentations
            .state()
            .subtree_messaging
            .is_empty()
    );
    assert!(
        control
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .is_empty()
    );
    root.thread.shutdown_and_wait().await.expect("shutdown");
}

#[tokio::test]
async fn unsupported_settings_do_not_become_volatile_user_grants() {
    let config = test_config().await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        /*state_db*/ None,
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .expect("root");
    let control = root.thread.session.services.agent_control.clone();
    let permission = control.acquire_messaging_permission_transaction().await;
    let error = control
        .persist_agent_send_setting_locked(
            AgentSendScope::Subtree {
                supervisor_thread_id: root.thread_id,
            },
            TargetMessageRouteMode::Enabled,
        )
        .await
        .expect_err("unsupported");
    assert!(matches!(error, CodexErr::UnsupportedOperation(_)));
    assert!(matches!(
        control
            .mailbox_send_permission_locked(ThreadId::new(), root.thread_id)
            .await,
        Err(CodexErr::UnsupportedOperation(_))
    ));
    assert!(
        control
            .wait_agent_presentations
            .state()
            .subtree_messaging
            .is_empty()
    );
    drop(permission);
    root.thread.shutdown_and_wait().await.expect("shutdown");
}
