use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;

use super::LocalThreadStore;
use super::live_writer;
use super::read_thread;
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
    let path = read_thread::resolve_rollout_path(store, thread_id, include_archived).await?;
    let Some(path) = path else {
        if live_writer::rollout_path(store, thread_id).await.is_ok() {
            return Ok(Vec::new());
        }
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    };
    read_thread::load_history_items(path.as_path()).await
}
