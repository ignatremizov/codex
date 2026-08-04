//! Canonical completion writes and unbounded artifact reads for the debug/test store.

use codex_protocol::models::ResponseItem;
use codex_rollout::persisted_rollout_items;

use super::InMemoryThreadStore;
use super::InMemoryThreadStoreFailure;
use super::history_mode_from_state;
use crate::AppendThreadItemsParams;
use crate::LoadSubAgentCompletionContextItemParams;
use crate::LoadSubAgentCompletionPresentationParams;
use crate::StoredSubAgentCompletionPresentation;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn append(
    store: &InMemoryThreadStore,
    params: AppendThreadItemsParams,
) -> ThreadStoreResult<()> {
    let mut state = store.state.lock().await;
    if !state.histories.contains_key(&params.thread_id) {
        return Err(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        });
    }
    let history_mode = history_mode_from_state(&state, params.thread_id);
    let items = persisted_rollout_items(&params.items, history_mode);
    state.calls.append_completion_items_and_flush += 1;
    state.calls.persist_thread += 1;
    if params.items.iter().any(|item| {
        matches!(
            item,
            codex_rollout::RolloutItem::AgentResponseObservation(_)
        )
    }) && let Some((remaining, failure)) = state.observation_barrier_failure.as_mut()
    {
        if *remaining == 0 {
            let failure = *failure;
            state.observation_barrier_failure = None;
            state.fail_next_operation = Some(failure);
        } else {
            *remaining -= 1;
        }
    }
    let failure = match state.fail_next_operation {
        Some(
            failure @ (InMemoryThreadStoreFailure::SubAgentCompletionAppend
            | InMemoryThreadStoreFailure::SubAgentCompletionPrefix
            | InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush),
        ) => {
            state.fail_next_operation = None;
            Some(failure)
        }
        _ => None,
    };
    let retained = match failure {
        Some(InMemoryThreadStoreFailure::SubAgentCompletionAppend) => 0,
        Some(InMemoryThreadStoreFailure::SubAgentCompletionPrefix) => items.len().min(1),
        _ => items.len(),
    };
    state
        .histories
        .entry(params.thread_id)
        .or_default()
        .extend(items.into_iter().take(retained));
    state.calls.flush_thread += retained.max(1);
    if let Some(failure) = failure {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "injected in-memory thread-store {} failure",
                failure.operation()
            ),
        });
    }
    Ok(())
}

pub(super) async fn context_item(
    store: &InMemoryThreadStore,
    params: LoadSubAgentCompletionContextItemParams,
) -> ThreadStoreResult<Option<ResponseItem>> {
    let mut state = store.state.lock().await;
    state.calls.load_sub_agent_completion_context_item += 1;
    let items = state
        .histories
        .get(&params.thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        })?;
    crate::completion_artifacts::context_item(std::slice::from_ref(items), &params.response_item_id)
}

pub(super) async fn presentation(
    store: &InMemoryThreadStore,
    params: LoadSubAgentCompletionPresentationParams,
) -> ThreadStoreResult<StoredSubAgentCompletionPresentation> {
    let mut state = store.state.lock().await;
    state.calls.load_sub_agent_completion_presentation += 1;
    let items = state
        .histories
        .get(&params.thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        })?;
    crate::completion_artifacts::presentation(
        std::slice::from_ref(items),
        &params.item_id,
        &params.turn_id,
    )
}
