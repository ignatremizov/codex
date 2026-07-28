use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;

use super::LocalThreadStore;
use super::live_writer;
use super::read_thread;
use super::thread_rollout_resolver;
use crate::LoadSubAgentCompletionContextItemParams;
use crate::LoadSubAgentCompletionPresentationParams;
use crate::StoredSubAgentCompletionPresentation;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn load_context_item(
    store: &LocalThreadStore,
    params: LoadSubAgentCompletionContextItemParams,
) -> ThreadStoreResult<Option<ResponseItem>> {
    let items = load_canonical_items(store, params.thread_id, params.include_archived).await?;
    crate::completion_artifacts::context_item(&items, &params.response_item_id)
}

pub(super) async fn load_presentation(
    store: &LocalThreadStore,
    params: LoadSubAgentCompletionPresentationParams,
) -> ThreadStoreResult<StoredSubAgentCompletionPresentation> {
    let items = load_canonical_items(store, params.thread_id, params.include_archived).await?;
    crate::completion_artifacts::presentation(&items, &params.item_id, &params.turn_id)
}

async fn load_canonical_items(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<Vec<Vec<RolloutItem>>> {
    let resolved = if include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, thread_id).await?
    };
    let Some(resolved) = resolved else {
        if live_writer::rollout_path(store, thread_id).await.is_ok() {
            return Ok(Vec::new());
        }
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    };
    let session_meta = codex_rollout::read_session_meta_line(resolved.path.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read completion history metadata: {err}"),
        })?;
    if session_meta.meta.id != thread_id {
        return Err(ThreadStoreError::Conflict {
            message: "completion history belongs to another thread".to_string(),
        });
    }
    if session_meta.meta.history_mode == ThreadHistoryMode::Legacy {
        let items = read_thread::load_history_items(resolved.path.as_path()).await?;
        return Ok(vec![items]);
    }
    let lineage = store.resolve_rollout_lineage(thread_id).await?;
    tokio::task::spawn_blocking(move || {
        let mut items = Vec::new();
        for segment in lineage.segments() {
            // Preserve source coordinates and original adjacency. Masking before concatenating
            // must not create a metadata/response pair that never existed canonically.
            items.push(
                super::model_context::rollback::read_canonical_segment(segment).map_err(|err| {
                    ThreadStoreError::Internal {
                        message: format!("failed to read completion history segment: {err}"),
                    }
                })?,
            );
        }
        Ok(items)
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("completion history reader failed: {err}"),
    })?
}
