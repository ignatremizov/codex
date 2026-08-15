use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ArchiveThreadsParams;
use crate::CreateProjectParams;
use crate::CreateThreadParams;
use crate::CreateThreadSectionParams;
use crate::CreatedProject;
use crate::DeleteThreadParams;
use crate::DeleteThreadSectionParams;
use crate::DeleteThreadsParams;
use crate::DeletedProject;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListProjectsParams;
use crate::ListThreadSectionsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadSubAgentCompletionContextItemParams;
use crate::LoadSubAgentCompletionPresentationParams;
use crate::LoadThreadHistoryParams;
use crate::MoveProjectParams;
use crate::MoveThreadToSectionParams;
use crate::PrepareForkParams;
use crate::PreparedFork;
use crate::ProjectMoveOutcome;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::RenameThreadSectionParams;
use crate::ResumeThreadParams;
use crate::RevertThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredModelContext;
use crate::StoredProject;
use crate::StoredProjectsPage;
use crate::StoredSubAgentCompletionPresentation;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::StoredThreadSection;
use crate::StoredThreadSectionsPage;
use crate::ThreadMetadataPatch;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;
use crate::UpdateProjectParams;
use crate::UpdateThreadMetadataParams;
use crate::UpdatedProject;

/// Future returned by [`ThreadStore`] operations.
pub type ThreadStoreFuture<'a, T> = Pin<Box<dyn Future<Output = ThreadStoreResult<T>> + Send + 'a>>;

/// Why thread persistence is being requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistContext {
    /// Standard persistence makes the thread and all queued items durable and readable.
    Standard,
    /// A turn is about to begin sampling after its input has been recorded.
    TurnStart,
}

/// Exclusive writer reservations held across a higher-level lifecycle transaction.
///
/// Store implementations keep their concrete lock guards private. Callers retain this opaque value
/// until the lifecycle transaction commits or rolls back, preventing any reserved thread from
/// acquiring a live writer in the interim.
pub struct ThreadWriterReservation {
    _guard: Box<dyn Send>,
}

impl ThreadWriterReservation {
    pub(crate) fn new(guard: impl Send + 'static) -> Self {
        Self {
            _guard: Box::new(guard),
        }
    }
}

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

    /// Stages host-owned metadata for a thread ID reserved before Core starts the thread.
    ///
    /// The entry remains in memory until the first successful metadata update for that thread.
    /// Callers must remove it if startup fails before the store opens a live thread.
    fn stage_pending_thread_metadata(
        &self,
        _thread_id: ThreadId,
        _patch: ThreadMetadataPatch,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "stage_pending_thread_metadata",
            })
        })
    }

    /// Removes host-owned metadata staged for a reserved thread ID.
    fn remove_pending_thread_metadata(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "remove_pending_thread_metadata",
            })
        })
    }

    /// Reopens an existing thread for live appends.
    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Exclusively reserves live-writer ownership for every supplied thread.
    ///
    /// Implementations must either reserve the complete set or release any partial acquisition
    /// before returning an error. The caller holds the returned value across its external
    /// lifecycle transaction.
    fn reserve_thread_writers(
        &self,
        thread_ids: Vec<ThreadId>,
    ) -> ThreadStoreFuture<'_, ThreadWriterReservation>;

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
    ///
    /// Standard persistence must complete before returning. Turn-start persistence may complete
    /// in the background when the implementation enqueues it before returning, fences it with
    /// subsequent flush or shutdown operations, and surfaces failures through those operations.
    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()>;

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

    /// Loads persisted non-paginated history for resume, fork, and memory jobs.
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory>;

    /// Loads full canonical rollout history for exact rollback and commit verification.
    ///
    /// Unlike non-paginated full-history reads, this internal operation remains available for paginated
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

    /// Reverts a paginated thread's durable history so it ends immediately before
    /// `before_turn_id`.
    ///
    /// Callers must close the thread's live writer first. The logical thread id and semantic
    /// metadata stay unchanged.
    ///
    /// Stores without paginated revert support can retain this default implementation.
    fn revert_thread(&self, _params: RevertThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "revert_thread",
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

    /// Whether this store can discover and manage independently persisted thread sections.
    fn supports_thread_sections(&self) -> bool {
        false
    }

    /// Lists independently persisted thread sections.
    fn list_thread_sections(
        &self,
        _params: ListThreadSectionsParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSectionsPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/list",
            })
        })
    }

    /// Creates a custom thread section with a stable, server-assigned identity.
    fn create_thread_section(
        &self,
        _params: CreateThreadSectionParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSection> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/create",
            })
        })
    }

    /// Renames a custom thread section, returning `None` when it does not exist.
    fn rename_thread_section(
        &self,
        _params: RenameThreadSectionParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThreadSection>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/update",
            })
        })
    }

    /// Deletes a custom thread section and reports whether it existed.
    fn delete_thread_section(
        &self,
        _params: DeleteThreadSectionParams,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/delete",
            })
        })
    }

    /// Whether this store supports durable host-owned projects.
    fn supports_projects(&self) -> bool {
        false
    }

    fn list_projects(
        &self,
        _params: ListProjectsParams,
    ) -> ThreadStoreFuture<'_, StoredProjectsPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/list",
            })
        })
    }

    fn read_project(&self, _project_id: String) -> ThreadStoreFuture<'_, Option<StoredProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/read",
            })
        })
    }

    fn create_project(
        &self,
        _params: CreateProjectParams,
    ) -> ThreadStoreFuture<'_, CreatedProject> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/create",
            })
        })
    }

    fn update_project(
        &self,
        _params: UpdateProjectParams,
    ) -> ThreadStoreFuture<'_, Option<UpdatedProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/update",
            })
        })
    }

    fn move_project(
        &self,
        _params: MoveProjectParams,
    ) -> ThreadStoreFuture<'_, Option<ProjectMoveOutcome>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/move",
            })
        })
    }

    fn delete_project(&self, _project_id: String) -> ThreadStoreFuture<'_, Option<DeletedProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/delete",
            })
        })
    }

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

    /// Lists bounded ordinary and realtime thread history in rollout order.
    fn list_timeline(
        &self,
        _params: crate::ListTimelineParams,
    ) -> ThreadStoreFuture<'_, crate::TimelinePage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/timeline/list",
            })
        })
    }

    /// Applies a literal metadata patch and returns the updated thread when one was materialized.
    ///
    /// `None` means the update succeeded without materializing a thread, for example because the
    /// implementation filtered the patch to a no-op. Callers that require a `StoredThread` must
    /// perform a fallback read.
    ///
    /// Implementations should apply the supplied fields directly. Policy such as deciding whether
    /// an append-derived preview should be emitted belongs above the store.
    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>>;

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
