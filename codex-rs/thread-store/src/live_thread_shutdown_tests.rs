use super::*;
use crate::InMemoryThreadStore;
use crate::InMemoryThreadStoreFailure;
use crate::LocalThreadStoreConfig;
use crate::ThreadPersistenceMetadata;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::UserMessageEvent;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

async fn live_thread(store: Arc<dyn ThreadStore>, cwd: &Path) -> LiveThread {
    let thread_id = ThreadId::new();
    LiveThread::create(
        store,
        CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "shutdown-test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: Default::default(),
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(cwd.to_path_buf()),
                model_provider: "test".to_string(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await
    .expect("create live thread")
}

fn user_message() -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "preserve shutdown history".to_string(),
        ..Default::default()
    }))
}

#[tokio::test]
async fn metadata_failure_does_not_close_writer_before_retry() {
    let home = TempDir::new().expect("home");
    let store = Arc::new(InMemoryThreadStore::default());
    let thread = live_thread(store.clone(), home.path()).await;
    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadMetadataUpdate)
        .await;
    thread
        .append_items(&[user_message()])
        .await
        .expect_err("leave derived metadata pending");
    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadMetadataUpdate)
        .await;
    thread
        .shutdown_durably()
        .await
        .expect_err("metadata failure precedes writer closure");
    assert_eq!(store.calls().await.shutdown_thread, 0);
    thread
        .shutdown_durably()
        .await
        .expect("retry pending metadata and writer close");
    assert_eq!(store.calls().await.shutdown_thread, 1);
}

#[tokio::test]
async fn failed_final_flush_retains_writer_lease_until_retry_succeeds() {
    let home = TempDir::new().expect("home");
    let store = Arc::new(LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(),
            sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
            default_model_provider_id: "test".to_string(),
        },
        /*state_db*/ None,
    ));
    let thread = live_thread(store.clone(), home.path()).await;
    let rollout_path = thread
        .local_rollout_path()
        .await
        .expect("local rollout lookup")
        .expect("local rollout");
    tokio::fs::create_dir_all(&rollout_path)
        .await
        .expect("block final file creation");
    // Queue canonical history directly: derived metadata is covered independently above.
    store
        .append_items(AppendThreadItemsParams {
            thread_id: thread.thread_id,
            items: vec![user_message()],
        })
        .await
        .expect_err("failed append flush retains queued history for shutdown retry");
    thread
        .shutdown_durably()
        .await
        .expect_err("final flush must report the file error");
    assert!(
        store
            .reserve_thread_writers(vec![thread.thread_id])
            .await
            .is_err(),
        "failed shutdown must retain the writer lease"
    );
    tokio::fs::remove_dir(&rollout_path)
        .await
        .expect("remove empty obstruction");
    thread
        .shutdown_durably()
        .await
        .expect("retry flush and release writer lease");
    let _reservation = store
        .reserve_thread_writers(vec![thread.thread_id])
        .await
        .expect("successful store shutdown releases the writer lease");
    let contents = tokio::fs::read_to_string(rollout_path)
        .await
        .expect("read flushed rollout");
    assert!(contents.contains("preserve shutdown history"));
}

#[tokio::test]
async fn init_guard_discard_durably_retains_handle_after_failure() {
    let home = TempDir::new().expect("home");
    let store = Arc::new(InMemoryThreadStore::default());
    let thread = live_thread(store.clone(), home.path()).await;
    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadDiscard)
        .await;
    let mut guard = LiveThreadInitGuard::new(Some(thread));

    guard
        .discard_durably()
        .await
        .expect_err("failed discard must remain observable");
    assert!(guard.as_ref().is_some());

    guard
        .discard_durably()
        .await
        .expect("retry releases the retained live thread");
    assert!(guard.as_ref().is_none());
}

#[tokio::test]
async fn init_guard_discard_durably_retains_acquisition_failure() {
    let mut guard = LiveThreadInitGuard::default();
    guard
        .acquire(async {
            Err(ThreadStoreError::Internal {
                message: "acquisition failed".to_string(),
            })
        })
        .await
        .expect_err("acquisition failure must be observable");

    guard
        .discard_durably()
        .await
        .expect_err("failed acquisition must remain retryable evidence");
}
