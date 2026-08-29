use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::InMemoryThreadStore;
use crate::DeleteThreadParams;
use crate::DeleteThreadsParams;
use crate::ThreadStore;

#[tokio::test]
async fn singular_deletion_preserves_graph_while_explicit_bulk_remains_strict()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::TempDir::new()?;
    let state = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test".to_string(),
    )
    .await?;
    let store = InMemoryThreadStore::default().with_state_db(Some(state.clone()));
    let root = ThreadId::new();
    let child = ThreadId::new();
    for thread_id in [root, child] {
        let metadata = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            home.path().join(format!("{thread_id}.jsonl")),
            Utc::now(),
            SessionSource::Cli,
        )
        .build("test");
        state.upsert_thread(&metadata).await?;
    }
    state
        .upsert_thread_spawn_edge(root, child, DirectionalThreadSpawnEdgeStatus::Open)
        .await?;
    let child_metadata = state.get_thread(child).await?;

    ThreadStore::delete_thread(&store, DeleteThreadParams { thread_id: root }).await?;
    assert_eq!(state.get_thread(root).await?, None);
    assert_eq!(state.get_thread(child).await?, child_metadata);
    assert_eq!(state.find_thread_spawn_parent(child).await?, Some(root));

    ThreadStore::delete_threads(
        &store,
        DeleteThreadsParams {
            thread_ids: vec![root],
        },
    )
    .await?;
    assert_eq!(state.find_thread_spawn_parent(child).await?, None);
    assert_eq!(state.get_thread(child).await?, child_metadata);
    Ok(())
}
