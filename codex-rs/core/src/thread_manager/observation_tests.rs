use super::*;
use crate::config::test_config;
use codex_history::CompactedItem;
use codex_protocol::protocol::UserMessageEvent;
use futures::FutureExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn close_epochs_share_the_restoration_gate_and_notify_existing_waiters() {
    let config = test_config().await;
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider,
    );
    let thread_id = ThreadId::new();
    let other_id = ThreadId::new();
    let original_lock = manager.state.v2_spawn_resume_lock(thread_id);
    let observer_lock = manager.state.agent_lifecycle_lock(thread_id);
    assert!(Arc::ptr_eq(&original_lock, &observer_lock));
    let guard = original_lock.lock_owned().await;
    assert!(observer_lock.try_lock_owned().is_err());
    let epoch = manager.state.agent_lifecycle_generation(thread_id);
    let other_epoch = manager.state.agent_lifecycle_generation(other_id);
    let changed = manager.state.wait_for_agent_lifecycle_change();

    manager.state.advance_agent_lifecycle_generation(thread_id);

    // Even a waiter first polled after the close must see this notification.
    assert_eq!(changed.now_or_never(), Some(()));
    assert!(
        !manager
            .state
            .agent_lifecycle_generation_is_current(thread_id, epoch)
    );
    assert!(
        manager
            .state
            .agent_lifecycle_generation_is_current(other_id, other_epoch)
    );
    let next_epoch = manager.state.agent_lifecycle_generation(thread_id);
    assert_eq!(
        (
            manager
                .state
                .with_current_agent_lifecycle_generation(thread_id, epoch, || "stale"),
            manager.state.with_current_agent_lifecycle_generation(
                thread_id,
                next_epoch,
                || "current"
            ),
            manager.state.with_current_agent_lifecycle_generation(
                other_id,
                other_epoch,
                || "unrelated"
            ),
        ),
        (None, Some("current"), Some("unrelated")),
    );
    drop(guard);
    let reopened_guard = manager
        .state
        .agent_lifecycle_lock(thread_id)
        .lock_owned()
        .await;
    assert!(
        manager
            .state
            .agent_lifecycle_generation_is_current(thread_id, next_epoch)
    );
    drop(reopened_guard);
}

#[tokio::test]
async fn canonical_history_keeps_pre_checkpoint_artifacts_in_both_history_modes() {
    for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let home = tempfile::tempdir().expect("temporary home");
        let mut config = test_config().await;
        config.codex_home = home.path().to_path_buf().try_into().expect("absolute home");
        config.cwd = config.codex_home.clone();
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            Arc::new(EnvironmentManager::default_for_tests()),
        );
        let started = manager
            .start_thread(StartThreadOptions {
                history_mode: Some(history_mode),
                environments: Some(Vec::new()),
                ..StartThreadOptions::new(config)
            })
            .await
            .expect("start persisted thread");
        started.thread.ensure_rollout_materialized().await;
        let evidence = vec![
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "evidence before the latest checkpoint".to_string(),
                ..Default::default()
            })),
            RolloutItem::Compacted(CompactedItem {
                replacement_history: Some(Vec::new()),
                ..Default::default()
            }),
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "evidence after the latest checkpoint".to_string(),
                ..Default::default()
            })),
        ];
        started
            .thread
            .append_rollout_items(&evidence)
            .await
            .expect("append evidence");
        started
            .thread
            .flush_rollout()
            .await
            .expect("flush evidence");
        let params = LoadThreadHistoryParams {
            thread_id: started.thread_id,
            include_archived: true,
        };
        let artifacts = manager
            .state
            .thread_store
            .load_canonical_artifact_segments(params)
            .await
            .expect("load full raw artifacts");
        let expected_suffix = serde_json::to_value(&evidence).expect("serialize expected history");
        let segment = artifacts.segments.last().expect("local artifact segment");
        assert_eq!(
            serde_json::to_value(&segment[segment.len() - evidence.len()..])
                .expect("serialize raw"),
            expected_suffix,
        );
        started
            .thread
            .shutdown_and_wait()
            .await
            .expect("shutdown thread");
    }
}
