use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ArchiveThreadsParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::DeleteThreadsParams;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadSubAgentCompletionContextItemParams;
use crate::LoadSubAgentCompletionPresentationParams;
use crate::LoadThreadHistoryParams;
use crate::MoveThreadToSectionParams;
use crate::PrepareForkParams;
use crate::PreparedFork;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredModelContext;
use crate::StoredSubAgentCompletionPresentation;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;
use crate::UpdateThreadMetadataParams;

/// Future returned by [`ThreadStore`] operations.
pub type ThreadStoreFuture<'a, T> = Pin<Box<dyn Future<Output = ThreadStoreResult<T>> + Send + 'a>>;

/// Storage-neutral thread persistence boundary.
pub trait ThreadStore: Any + Send + Sync {
    /// Return this store as [`Any`] for implementation-owned escape hatches.
    fn as_any(&self) -> &dyn Any;

    /// Returns the history mode to use when history does not carry a persisted mode.
    ///
    /// The default is legacy so existing stores stay compatible. Stores whose durable contract is
    /// already paginated should override this instead of relying on core to infer storage behavior.
    fn default_history_mode(&self) -> ThreadHistoryMode {
        ThreadHistoryMode::Legacy
    }

    /// Creates a new live thread.
    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Reopens an existing thread for live appends.
    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Appends raw rollout items to a live thread.
    ///
    /// Implementations should apply the shared rollout persistence policy before writing durable
    /// replay history and before updating any implementation-owned projections.
    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()>;

    /// Appends canonical history and completes its durability barrier.
    ///
    /// Rebuildable projection failures must be retained or logged separately after the canonical
    /// history commit succeeds. Callers remain responsible for ordering multi-record batches so
    /// any durable prefix is replay-safe after interruption.
    fn append_items_and_flush(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        let thread_id = params.thread_id;
        Box::pin(async move {
            self.append_items(params).await?;
            self.flush_thread(thread_id).await
        })
    }

    /// Materializes the thread if persistence is lazy, then persists all queued items.
    fn persist_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Flushes all queued items and returns once canonical history is durable/readable.
    ///
    /// Rebuildable projection failures must not be returned after the canonical durability barrier
    /// has succeeded; implementations should retain or log those failures for a later retry.
    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Flushes pending items and closes the live thread writer.
    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Discards the live thread writer without forcing pending in-memory items to become durable.
    ///
    /// Core calls this when session initialization fails after a live writer has been created.
    /// Implementations should release any live writer resources for the thread while preserving
    /// already-durable thread data.
    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Loads persisted legacy history for resume, fork, and memory jobs.
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory>;

    /// Loads full canonical rollout history for exact rollback and commit verification.
    ///
    /// Unlike legacy full-history reads, this internal operation remains available for paginated
    /// threads because rollback needs absolute canonical item positions.
    fn load_rollback_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        self.load_history(params)
    }

    /// Locates a trusted completion-context item by its stable reserved identity.
    ///
    /// This lookup spans canonical history even when paginated model-context reads use a bounded
    /// suffix, and returns only the matching artifact.
    fn load_sub_agent_completion_context_item(
        &self,
        params: LoadSubAgentCompletionContextItemParams,
    ) -> ThreadStoreFuture<'_, Option<ResponseItem>>;

    /// Locates a canonical completion presentation and the queried turn's lifecycle.
    ///
    /// This lookup spans canonical history so commit-unknown retries remain idempotent across
    /// compaction boundaries.
    fn load_sub_agent_completion_presentation(
        &self,
        params: LoadSubAgentCompletionPresentationParams,
    ) -> ThreadStoreFuture<'_, StoredSubAgentCompletionPresentation>;

    /// Loads the persisted rollout items needed to reconstruct the latest model-visible context.
    ///
    /// Implementations that cannot perform a targeted read may return the full persisted history.
    fn load_latest_model_context(
        &self,
        _params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "load_latest_model_context",
            })
        })
    }

    /// Freezes source history and model context used to initialize a referenced fork.
    ///
    /// Stores without reference-backed fork support can retain this default implementation.
    fn prepare_fork(&self, _params: PrepareForkParams) -> ThreadStoreFuture<'_, PreparedFork> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "prepare_fork",
            })
        })
    }

    /// Reads a thread summary and optionally its persisted history.
    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread>;

    /// Reads a rollout-backed thread by path when the store supports path-addressed lookups.
    ///
    /// Deprecated: new callers should use [`ThreadStore::read_thread`] instead.
    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread>;

    /// Lists stored threads matching the supplied filters.
    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage>;

    /// Whether paginated threads can hydrate durable history through turn and item lists.
    fn supports_paginated_history_lists(&self) -> bool {
        false
    }

    /// Searches stored threads and returns search-only preview metadata.
    fn search_threads(
        &self,
        _params: SearchThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadSearchPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/search",
            })
        })
    }

    /// Searches visible message occurrences within one paginated thread.
    fn search_thread_occurrences(
        &self,
        _params: SearchThreadOccurrencesParams,
    ) -> ThreadStoreFuture<'_, ThreadOccurrenceSearchPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/searchOccurrences",
            })
        })
    }

    /// Lists turns within a stored thread.
    fn list_turns(&self, _params: ListTurnsParams) -> ThreadStoreFuture<'_, TurnPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "list_turns",
            })
        })
    }

    /// Lists persisted items within a stored thread, optionally filtered to a turn.
    fn list_items(&self, _params: ListItemsParams) -> ThreadStoreFuture<'_, ItemPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "list_items",
            })
        })
    }

    /// Applies a literal metadata patch and returns the updated thread.
    ///
    /// Implementations should apply the supplied fields directly. Policy such as deciding whether
    /// an append-derived preview should be emitted belongs above the store.
    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread>;

    /// Moves a thread to, within, or out of a server-ordered section.
    fn move_thread_to_section(
        &self,
        _params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/section/move",
            })
        })
    }

    /// Archives a thread.
    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Archives threads in order, returning the successfully archived thread ids.
    ///
    /// The first thread must archive successfully; later failures are best effort.
    fn archive_threads(
        &self,
        params: ArchiveThreadsParams,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            let mut archived_thread_ids = Vec::new();
            for thread_id in params.thread_ids {
                match self.archive_thread(ArchiveThreadParams { thread_id }).await {
                    Ok(()) => archived_thread_ids.push(thread_id),
                    Err(err) if archived_thread_ids.is_empty() => return Err(err),
                    Err(err) => tracing::warn!("failed to archive thread {thread_id}: {err}"),
                }
            }
            Ok(archived_thread_ids)
        })
    }

    /// Unarchives a thread and returns its updated metadata.
    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread>;

    /// Deletes a thread's persisted rollout data and associated metadata.
    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Deletes threads in order, treating already-missing members as deleted.
    ///
    /// Stores with request-scoped delete preflight should override this instead of repeating
    /// that work through [`ThreadStore::delete_thread`].
    fn delete_threads(&self, params: DeleteThreadsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            for thread_id in params.thread_ids {
                match self.delete_thread(DeleteThreadParams { thread_id }).await {
                    Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        })
    }
}
