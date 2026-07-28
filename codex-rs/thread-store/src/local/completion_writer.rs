//! One-shot canonical completion writes. Failed prefixes never enter the ordinary retry queue.

use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::persisted_rollout_items;
use tracing::warn;

use super::LocalThreadStore;
use super::live_writer;
use crate::AppendThreadItemsParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn append(
    store: &LocalThreadStore,
    params: AppendThreadItemsParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let _writer_guard = store.live_writer_locks.lock(thread_id).await;
    let (recorder, rollout_id, history_mode) =
        live_writer::live_writer_parts(store, thread_id).await?;
    let items = persisted_rollout_items(&params.items, history_mode);
    // The single-record barrier deliberately refuses to materialize a missing SessionMeta.
    recorder.persist().await.map_err(io_error)?;
    if items.is_empty() {
        // Recovery under a new exclusive writer must acknowledge existing canonical records,
        // even when lookup found the whole batch and there is nothing left to append.
        recorder.flush().await.map_err(io_error)?;
    }
    for item in &items {
        recorder
            .record_canonical_item_and_flush(item)
            .await
            .map_err(io_error)?;
    }
    if history_mode == ThreadHistoryMode::Paginated
        && let Err(err) = super::thread_history_materialization::materialize_to_sqlite(
            store,
            rollout_id,
            recorder.rollout_path(),
        )
        .await
    {
        warn!("failed to project canonical completion for {thread_id}: {err}");
    }
    Ok(())
}

fn io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}
