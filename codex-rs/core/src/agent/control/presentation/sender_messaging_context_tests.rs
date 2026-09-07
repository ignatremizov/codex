#![expect(
    clippy::await_holding_invalid_type,
    reason = "parameterized read-only projection tests exclude lifecycle refreshes across awaits"
)]

use super::*;
use crate::ThreadManager;
use crate::UserAgentReplyRouteMode;
use crate::UserAgentSpawnOptions;
use crate::config::test_config;
use crate::init_state_db;
use crate::thread_manager::StartThreadOptions;
use codex_login::CodexAuth;
use codex_protocol::protocol::ThreadHistoryMode;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use test_case::test_case;

async fn histories(threads: &[Arc<crate::CodexThread>]) -> Vec<Vec<ResponseItem>> {
    let mut histories = Vec::new();
    for thread in threads {
        histories.push(
            thread
                .session
                .clone_history()
                .await
                .raw_items()
                .cloned()
                .collect(),
        );
    }
    histories
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn repeated_sender_snapshots_are_read_only_but_policy_mutation_delivers_notices(
    history_mode: ThreadHistoryMode,
) {
    // Real idle sessions make an accidental inject_no_new_turn observable in durable
    // history, even when it does not create a model request or wake the thread.
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
    let mut options = StartThreadOptions::new(config);
    options.history_mode = Some(history_mode);
    let root = manager.start_thread(options).await.expect("root");
    let control = root.thread.session.services.agent_control.clone();
    let sender = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("sender");
    let peer = root
        .thread
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("peer");
    let sender = manager
        .get_thread(sender.target_thread_id)
        .await
        .expect("sender runtime");
    let peer = manager
        .get_thread(peer.target_thread_id)
        .await
        .expect("peer runtime");
    root.thread
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await
        .expect("enable sibling messages");
    // Publishing a runtime schedules a detached discovery refresh. Exclude those
    // lifecycle writers while staging undelivered authority and proving that request
    // projection itself neither delivers notices nor mutates the routing state.
    let permission_transaction = control.acquire_messaging_permission_transaction().await;
    let source = sender.session.presentation_id();
    let target = peer.session.presentation_id();
    let enabled = control
        .messaging_context_snapshot(source)
        .await
        .expect("enabled context");
    assert!(enabled.allowed_targets.contains(&target.thread_id));
    let TargetMessageAdmission::Wake(reservation) = control
        .target_message_admission(
            target,
            source,
            "source-turn",
            /*observer_active_turn_id*/ None,
            /*observer_last_terminal_turn_id*/ None,
            TargetMessageAdmissionMode::SeparateTurn,
        )
        .expect("reserve wake")
    else {
        panic!("expected wake reservation");
    };
    // Deterministic boundary: authority has changed but lifecycle refresh has not
    // delivered it yet. Requests must see it without completing the mutation's work.
    {
        let mut state = control.wait_agent_presentations.state();
        state
            .subtree_messaging
            .get_mut(&root.thread.session.presentation_id())
            .expect("root policy")
            .1 = TargetMessageRouteMode::Disabled;
    }
    let threads = [root.thread.clone(), sender.clone(), peer.clone()];
    let before_history = histories(&threads).await;
    let before_authority =
        SenderAuthority::capture(&control.wait_agent_presentations.state(), source);
    let (before_routes, before_pending) = {
        let state = control.wait_agent_presentations.state();
        (
            state.inherited_message_routes.clone(),
            state.pending_messaging_context.clone(),
        )
    };
    let before_ops = manager.captured_ops().len();
    let mut expected = before_history[1].clone();
    for _ in 0..3 {
        let snapshot = control
            .messaging_context_snapshot(source)
            .await
            .expect("read-only context");
        assert!(!snapshot.allowed_targets.contains(&target.thread_id));
        let mut projected = before_history[1].clone();
        snapshot.reconcile(&mut projected);
        snapshot.reconcile(&mut expected);
        assert_eq!(
            projected, expected,
            "unchanged requests have stable items and metadata"
        );
        assert!(projected.iter().any(|item| {
            notice_parts(item).is_some_and(|(_, text)| text.starts_with("User disabled send_input"))
        }));
        assert_eq!(
            histories(&threads).await,
            before_history,
            "no injection into any runtime"
        );
        let state = control.wait_agent_presentations.state();
        assert!(SenderAuthority::capture(&state, source) == before_authority);
        assert_eq!(state.inherited_message_routes, before_routes);
        assert_eq!(state.pending_messaging_context, before_pending);
    }
    assert_eq!(manager.captured_ops().len(), before_ops);
    assert!(
        control.target_message_wake_is_current(target, source, "source-turn", reservation),
        "projection must not revoke the reservation using its filtered policy view"
    );
    drop(permission_transaction);
    root.thread
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
        .await
        .expect("mutation delivers current context");
    assert!(!control.target_message_wake_is_current(target, source, "source-turn", reservation));
    let after_history = histories(&threads).await;
    for index in [1, 2] {
        assert_ne!(after_history[index], before_history[index]);
        assert!(after_history[index].iter().any(|item| {
            notice_parts(item).is_some_and(|(_, text)| text.starts_with("User disabled send_input"))
        }));
    }
    let delivered = control
        .messaging_context_snapshot(source)
        .await
        .expect("delivered context");
    assert!(
        delivered
            .notices
            .iter()
            .all(|item| after_history[1].contains(item)),
        "request projection reuses the exact delivered notice identity"
    );
    let mut once = after_history[1].clone();
    delivered.reconcile(&mut once);
    let mut twice = once.clone();
    control
        .messaging_context_snapshot(source)
        .await
        .expect("retry")
        .reconcile(&mut twice);
    assert_eq!(twice, once);
    assert_eq!(histories(&threads).await, after_history);
    assert_eq!(
        manager.captured_ops().len(),
        before_ops,
        "notice delivery is nonwaking"
    );
}

#[tokio::test]
async fn unbound_snapshot_neutralizes_old_notices_without_initializing_ownership() {
    let control = AgentControl::default();
    let sender = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let old_notice = ContextualUserFragment::into(PermissionNotice {
        key: "subtree.old-runtime".to_string(),
        text: "User enabled send_input within an old subtree.".to_string(),
    });
    let original = vec![old_notice];
    let mut projected = original.clone();
    let snapshot = control
        .messaging_context_snapshot(sender)
        .await
        .expect("unbound no-op");
    snapshot.reconcile(&mut projected);
    assert_eq!(
        projected
            .iter()
            .filter_map(notice_parts)
            .collect::<Vec<_>>(),
        vec![("baseline", "No live user messaging overrides are active.")]
    );
    let mut retried = projected.clone();
    control
        .messaging_context_snapshot(sender)
        .await
        .expect("retry")
        .reconcile(&mut retried);
    assert_eq!(retried, projected);
    assert_ne!(original, projected);
    let state = control.wait_agent_presentations.state();
    assert!(state.subtree_messaging.is_empty());
    assert!(state.inherited_message_routes.is_empty());
    assert!(state.pending_messaging_context.is_empty());
}
