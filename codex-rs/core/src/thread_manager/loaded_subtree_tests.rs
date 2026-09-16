use super::*;
use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::build_models_manager;
use codex_extension_api::empty_extension_registry;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use core_test_support::PathExt;
use futures::FutureExt;
use pretty_assertions::assert_eq;

async fn fixture() -> (
    tempfile::TempDir,
    Config,
    ThreadManager,
    Arc<InMemoryThreadStore>,
) {
    let home = tempfile::tempdir().expect("test home");
    let mut config = test_config().await;
    config.codex_home = home.path().abs();
    config.cwd = config.codex_home.abs();
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test"));
    let store = Arc::new(InMemoryThreadStore::default());
    let manager = ThreadManager::new(
        &config,
        Arc::clone(&auth),
        build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        store.clone(),
        /*agent_graph_store*/ None,
        "subtree-unload-test".to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    (home, config, manager, store)
}

async fn child(manager: &ThreadManager, config: &Config, parent: ThreadId) -> ThreadId {
    let mut options = StartThreadOptions::new(config.clone());
    options.session_source = Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    }));
    manager
        .start_thread(options)
        .await
        .expect("child")
        .thread_id
}

#[tokio::test]
async fn child_selection_captures_owning_root_and_fences_membership() {
    let (_home, config, manager, _store) = fixture().await;
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("root")
        .thread_id;
    let child_id = child(&manager, &config, root).await;
    let grandchild = child(&manager, &config, child_id).await;
    let mut subtree = manager
        .prepare_subtree_unload(grandchild)
        .await
        .expect("capture");
    assert_eq!(
        (subtree.root_thread_id(), subtree.thread_ids()),
        (root, [root, child_id, grandchild].as_slice())
    );
    let mut new_membership = Box::pin(manager.state.acquire_agent_membership_lifecycle(child_id));
    assert!(new_membership.as_mut().now_or_never().is_none());
    // Keep polling the queued FIFO waiter when unload releases its lifecycle locks.
    // Otherwise that waiter reserves the child lock and blocks unload's final reacquisition.
    let (unloaded, new_membership) = tokio::join!(subtree.shutdown_and_remove(), new_membership);
    let unloaded = unloaded.expect("durable unload");
    assert!(new_membership.is_err());
    assert_eq!(unloaded, vec![root, child_id, grandchild]);
    assert!(manager.list_thread_ids().await.is_empty());
    drop(subtree);
    let retry = manager
        .prepare_subtree_unload(grandchild)
        .await
        .expect("known unloaded child");
    assert_eq!(
        (retry.root_thread_id(), retry.thread_ids()),
        (root, [].as_slice())
    );
}

#[tokio::test]
async fn unloaded_parent_does_not_hide_loaded_descendants() {
    let (_home, config, manager, _store) = fixture().await;
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("root");
    let child_id = child(&manager, &config, root.thread_id).await;
    root.thread
        .shutdown_durably_and_wait()
        .await
        .expect("stop root");
    manager
        .remove_thread_if_current(&root.thread)
        .await
        .expect("remove root");
    let mut subtree = manager
        .prepare_subtree_unload(root.thread_id)
        .await
        .expect("capture orphan");
    assert_eq!(
        (subtree.root_thread_id(), subtree.thread_ids()),
        (root.thread_id, [child_id].as_slice())
    );
    assert_eq!(
        subtree.shutdown_and_remove().await.expect("unload child"),
        vec![child_id]
    );
}

#[tokio::test]
async fn failed_subtree_drain_retains_all_instances_until_retry() {
    let (_home, config, manager, store) = fixture().await;
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("root");
    let child_id = child(&manager, &config, root.thread_id).await;
    let child = manager.get_thread(child_id).await.expect("loaded child");
    let mut subtree = manager
        .prepare_subtree_unload(child_id)
        .await
        .expect("capture");
    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadShutdown)
        .await;
    assert!(subtree.shutdown_and_remove().await.is_err());
    assert!(Arc::ptr_eq(
        &manager
            .get_thread(root.thread_id)
            .await
            .expect("retained root"),
        &root.thread,
    ));
    assert!(Arc::ptr_eq(
        &manager.get_thread(child_id).await.expect("retained child"),
        &child,
    ));
    assert!(
        manager
            .state
            .acquire_agent_membership_lifecycle(child_id)
            .await
            .is_err()
    );
    // Completion watchers still need the ordinary lifecycle boundary to drain accepted work.
    drop(
        manager
            .state
            .acquire_live_agent_lifecycle(child_id)
            .await
            .expect("delivery lifecycle remains available"),
    );
    assert!(manager.remove_thread_if_current(&child).await.is_none());
    assert!(manager.agent_control().close_agent(child_id).await.is_err());
    assert_eq!(
        subtree.shutdown_and_remove().await.expect("retry"),
        vec![root.thread_id, child_id]
    );
    assert!(manager.list_thread_ids().await.is_empty());
}
