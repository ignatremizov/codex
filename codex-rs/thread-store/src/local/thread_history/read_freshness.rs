//! Keeps the exact lineage used by a paginated read fresh in the derived database.
//!
//! Reservations use the existing process-local lifecycle contract; they are not cross-process
//! writer ownership. Never acquire lifecycle reservations while holding a writer mutex.

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use tokio::sync::OwnedRwLockReadGuard;

use super::super::LocalThreadStore;
use super::super::rollout_lineage::RolloutLineage;
use super::super::thread_history_materialization;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) struct PreparedHistoryRead {
    pub(super) lineage: RolloutLineage,
    _reservations: Vec<OwnedRwLockReadGuard<()>>,
}

pub(super) async fn prepare(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
    operation: &'static str,
) -> ThreadStoreResult<PreparedHistoryRead> {
    for _ in 0..3 {
        validate_metadata(store, thread_id, include_archived, operation).await?;
        let discovered = store.resolve_rollout_lineage(thread_id).await?;
        let thread_ids = stable_thread_ids(&discovered, thread_id)?;
        let mut reservations = Vec::with_capacity(thread_ids.len());
        // Match batch lifecycle operations' ordering, including when revert gives a thread
        // multiple rollout IDs. Take shared reservations once per stable thread, not per file.
        for &id in &thread_ids {
            reservations.push(store.live_writer_locks.reserve_lifecycle(id).await);
        }
        validate_metadata(store, thread_id, include_archived, operation).await?;
        let lineage = store.resolve_rollout_lineage(thread_id).await?;
        if stable_thread_ids(&lineage, thread_id)? != thread_ids {
            // Revert may have changed the ancestry while reservations were being acquired.
            // Drop the complete set before retrying so lock order stays globally consistent.
            continue;
        }
        for (index, segment) in lineage.segments().iter().enumerate() {
            let writer_id = if index + 1 == lineage.segments().len() {
                thread_id
            } else {
                segment.rollout_id()
            };
            let _writer = store.live_writer_locks.lock(writer_id).await;
            thread_history_materialization::materialize_to_sqlite(
                store,
                segment.rollout_id(),
                &segment.rollout_path,
            )
            .await?;
        }
        return Ok(PreparedHistoryRead {
            lineage,
            _reservations: reservations,
        });
    }
    Err(ThreadStoreError::Conflict {
        message: format!("thread {thread_id} lineage changed while preparing its history read"),
    })
}

fn stable_thread_ids(
    lineage: &RolloutLineage,
    requested_thread_id: ThreadId,
) -> ThreadStoreResult<Vec<ThreadId>> {
    let mut ids = vec![requested_thread_id];
    for segment in lineage.segments() {
        ids.push(
            codex_rollout::thread_id_from_rollout_path(&segment.rollout_path).ok_or_else(|| {
                ThreadStoreError::Internal {
                    message: format!(
                        "cannot identify the owner of rollout {}",
                        segment.rollout_path.display()
                    ),
                }
            })?,
        );
    }
    ids.sort_unstable_by_key(ToString::to_string);
    ids.dedup();
    Ok(ids)
}

async fn validate_metadata(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
    operation: &'static str,
) -> ThreadStoreResult<()> {
    let state_db = store
        .state_db()
        .await
        .ok_or(ThreadStoreError::Unsupported { operation })?;
    let metadata = state_db
        .get_thread(thread_id)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read thread metadata: {err}"),
        })?
        .ok_or(ThreadStoreError::Unsupported { operation })?;
    if metadata.archived_at.is_some() && !include_archived {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {thread_id} is archived"),
        });
    }
    match metadata.history_mode {
        ThreadHistoryMode::Legacy => Err(ThreadStoreError::Unsupported { operation }),
        ThreadHistoryMode::Paginated => Ok(()),
    }
}
