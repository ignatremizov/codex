use codex_protocol::models::ResponseItem;
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
    Ok(crate::completion_artifacts::context_item(
        &items,
        &params.response_item_id,
    ))
}

pub(super) async fn load_presentation(
    store: &LocalThreadStore,
    params: LoadSubAgentCompletionPresentationParams,
) -> ThreadStoreResult<StoredSubAgentCompletionPresentation> {
    let items = load_canonical_items(store, params.thread_id, params.include_archived).await?;
    Ok(crate::completion_artifacts::presentation(
        &items,
        &params.item_id,
        &params.turn_id,
    ))
}

async fn load_canonical_items(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<Vec<RolloutItem>> {
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
    read_thread::load_history_items(resolved.path.as_path()).await
}
