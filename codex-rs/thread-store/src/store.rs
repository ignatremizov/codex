use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use crate::AddThreadAttachmentOutcome;
use crate::AddThreadAttachmentParams;
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
use crate::ListThreadAttachmentsParams;
use crate::ListThreadSectionsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadForkSourceByRolloutPathParams;
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
use crate::RemoveThreadAttachmentOutcome;
use crate::RemoveThreadAttachmentParams;
use crate::RenameThreadSectionParams;
use crate::ResumeThreadParams;
use crate::RevertThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredForkSource;
use crate::StoredModelContext;
use crate::StoredProject;
use crate::StoredProjectsPage;
use crate::StoredSubAgentCompletionPresentation;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::StoredThreadSection;
use crate::StoredThreadSectionsPage;
use crate::ThreadAttachmentPage;
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
    /// Accepted user input is being recorded before an active turn's next sampling request.
    /// This does not apply to tool outputs, cancellation, or task cleanup.
    SteeredUserInput,
}

impl PersistContext {
    /// Whether a store may enqueue this checkpoint before returning and fence it at a later
    /// durability barrier. Stores may still choose to persist synchronously.
    pub fn allows_background_persistence(self) -> bool {
        match self {
            Self::Standard => false,
            Self::TurnStart | Self::SteeredUserInput => true,
        }
    }
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

    /// Accepts immutable typed mail with a receiver-scoped idempotency key.
    ///
    /// Callers own send authority. Acceptance does not steer, queue a turn, or
    /// grant permission, and is separate from canonical history delivery.
    fn accept_mailbox_input(
        &self,
        _params: crate::AcceptMailboxInputParams,
    ) -> ThreadStoreFuture<'_, crate::StoredMailboxInput> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "accept_mailbox_input",
            })
        })
    }

    /// Accepts a wake subscription with graph authority captured by Core.
    ///
    /// Durable stores should persist the authority values beside the accepted message. Stores
    /// without this capability reject wake subscriptions instead of silently dropping the fence.
    fn accept_mailbox_input_with_authority(
        &self,
        _params: crate::AcceptMailboxInputParams,
        _authority: crate::MailboxFinalSubscriptionAuthority,
    ) -> ThreadStoreFuture<'_, crate::StoredMailboxInput> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "accept_mailbox_input_with_authority",
            })
        })
    }

    /// Reads the active mailbox final subscription for a canonical sender/receiver pair.
    fn lookup_active_mailbox_final_subscription(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
        _sender_thread_id: codex_protocol::ThreadId,
    ) -> ThreadStoreFuture<'_, Option<crate::MailboxFinalSubscription>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "lookup_active_mailbox_final_subscription",
            })
        })
    }

    /// Reads one mailbox final subscription by its accepted message identity, including terminal rows.
    fn lookup_mailbox_final_subscription(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
        _message_id: String,
    ) -> ThreadStoreFuture<'_, Option<crate::MailboxFinalSubscription>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "lookup_mailbox_final_subscription",
            })
        })
    }

    /// Lists pending/bound final subscriptions whose receiver or sender is this thread.
    fn read_active_mailbox_final_subscriptions_for_thread(
        &self,
        _thread_id: codex_protocol::ThreadId,
    ) -> ThreadStoreFuture<'_, Vec<crate::MailboxFinalSubscription>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "read_active_mailbox_final_subscriptions_for_thread",
            })
        })
    }

    /// Supersedes the active mailbox final subscription after a later ordinary observation.
    fn supersede_mailbox_final_subscription(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
        _sender_thread_id: codex_protocol::ThreadId,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "supersede_mailbox_final_subscription",
            })
        })
    }

    /// Supersedes one exact pending/bound final subscription.
    fn supersede_mailbox_final_subscription_message(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
        _message_id: String,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "supersede_mailbox_final_subscription_message",
            })
        })
    }

    /// Retires active subscriptions touching any endpoint in a closed/transferred subtree.
    fn supersede_mailbox_final_subscriptions_for_threads(
        &self,
        _thread_ids: Vec<codex_protocol::ThreadId>,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "supersede_mailbox_final_subscriptions_for_threads",
            })
        })
    }

    /// Marks the exact bound subscription delivered after the existing final-observation receipt
    /// is durable in the observer's canonical history.
    fn acknowledge_mailbox_final_subscription_delivery(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
        _message_id: String,
        _turn_id: String,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "acknowledge_mailbox_final_subscription_delivery",
            })
        })
    }

    /// Looks up immutable accepted input by receiver and submission key.
    ///
    /// Returns frozen attribution and the current state, without claiming or
    /// granting authority. Callers must verify sender, sender turn, and original
    /// typed input before reusing an existing submission for an idempotent retry.
    /// Acceptance retains its strict immutable-content comparison.
    fn lookup_mailbox_input<'a>(
        &'a self,
        _receiver_thread_id: codex_protocol::ThreadId,
        _submission_key: &'a str,
    ) -> ThreadStoreFuture<'a, Option<crate::StoredMailboxInput>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "lookup_mailbox_input",
            })
        })
    }

    /// Reads an existing invocation without reserving messages or establishing a claim.
    ///
    /// Returns its original selection, membership, delivery IDs, and current states,
    /// including empty and entirely terminal claims. Callers must validate the original
    /// selection on retries. This read grants no admission or delivery authority.
    fn lookup_mailbox_claim(
        &self,
        _invocation: crate::MailboxInvocation,
    ) -> ThreadStoreFuture<'_, Option<crate::MailboxClaim>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "lookup_mailbox_claim",
            })
        })
    }

    /// Reserves a fixed batch for a fully qualified receiver tool invocation.
    ///
    /// Retries recover the same members, stable delivery IDs, and current states.
    /// Empty claims remain empty. This does not acknowledge delivery or grant
    /// authority to inject payloads; callers own admission policy.
    fn claim_mailbox_input(
        &self,
        _params: crate::ClaimMailboxInputParams,
    ) -> ThreadStoreFuture<'_, crate::MailboxClaim> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "claim_mailbox_input",
            })
        })
    }

    /// Recovers exact artifacts using fixed-claim semantics, without acknowledging.
    ///
    /// May establish a new claim, including an empty one. Prefer claiming first
    /// and recovering with the same parameters, never a new invocation. Partial
    /// artifacts authorize neither repair nor resend; terminal states prevail.
    fn recover_mailbox_delivery(
        &self,
        _params: crate::ClaimMailboxInputParams,
    ) -> ThreadStoreFuture<'_, crate::RecoveredMailboxClaim> {
        Box::pin(async {
            Err(crate::ThreadStoreError::Unsupported {
                operation: "recover_mailbox_delivery",
            })
        })
    }

    /// Loads strict receiver-owned canonical history for mailbox evidence checks.
    ///
    /// Missing, foreign, malformed, or incomplete history must fail closed.
    fn load_mailbox_canonical_history(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
    ) -> ThreadStoreFuture<'_, Vec<codex_rollout::RolloutItem>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "load_mailbox_canonical_history",
            })
        })
    }

    /// Rejects under Core's permission/delivery arbitration after recovery.
    /// Cannot overwrite consumption or a different terminal rejection reason.
    fn reject_mailbox_input(
        &self,
        _params: crate::RejectMailboxInputParams,
    ) -> ThreadStoreFuture<'_, crate::StoredMailboxInput> {
        Box::pin(async {
            Err(crate::ThreadStoreError::Unsupported {
                operation: "reject_mailbox_input",
            })
        })
    }

    /// Acknowledges only members proven delivered in canonical receiver history.
    ///
    /// Returns the fixed batch with refreshed states. Unverified members remain
    /// claimed; consumed/rejected members must not be reinjected. Implementations
    /// verify prepared model context and original typed presentation, not merely
    /// a caller-supplied ID. Core owns append/retry and rejection serialization.
    fn reconcile_mailbox_delivery(
        &self,
        _params: crate::ReconcileMailboxDeliveryParams,
    ) -> ThreadStoreFuture<'_, crate::MailboxClaim> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "reconcile_mailbox_delivery",
            })
        })
    }

    /// Reads counts and notification progress without consuming or granting authority.
    fn read_mailbox_inventory(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
    ) -> ThreadStoreFuture<'_, crate::MailboxInventory> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "read_mailbox_inventory",
            })
        })
    }

    /// Tests whether any original-frontier mail is still pending for this receiver.
    ///
    /// Queries actual Pending rows at or before the snapshot frontier, not
    /// aggregate sender counts/maxima. A false result does not authorize
    /// cancellation: callers still need canonical absence under durable arbitration.
    fn has_pending_mailbox_inventory(
        &self,
        _notification: crate::MailboxInventoryNotification,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "has_pending_mailbox_inventory",
            })
        })
    }

    /// Recovers the single active preparation or fixes a new pending-mail snapshot.
    /// Its notification UUID is the immutable inventory turn ID, not a new task ID.
    fn prepare_mailbox_inventory(
        &self,
        _receiver_thread_id: codex_protocol::ThreadId,
    ) -> ThreadStoreFuture<'_, Option<crate::MailboxInventoryNotification>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "prepare_mailbox_inventory",
            })
        })
    }

    /// Recovers exact context against the original snapshot, without authorizing a wake.
    /// Caller holds durable delivery arbitration and preserves the snapshot for retries.
    fn recover_mailbox_inventory(
        &self,
        _notification: crate::MailboxInventoryNotification,
    ) -> ThreadStoreFuture<'_, crate::MailboxInventoryRecovery> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "recover_mailbox_inventory",
            })
        })
    }

    /// Acknowledges only canonical inventory proof; never consumes payloads.
    /// Retired originals require exact proof and watermark coverage. They never
    /// acknowledge a newer active preparation. No post-ack read is required.
    fn reconcile_mailbox_inventory(
        &self,
        _notification: crate::MailboxInventoryNotification,
    ) -> ThreadStoreFuture<'_, crate::MailboxInventoryAcknowledgement> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "reconcile_mailbox_inventory",
            })
        })
    }

    /// Cancels only the matching preparation after proving canonical absence.
    /// Caller must flush pending canonical writes and hold durable delivery
    /// arbitration through this entire call.
    /// Missing/unreadable history is not absence, and cancellation grants no wake.
    fn cancel_mailbox_inventory(
        &self,
        _notification: crate::MailboxInventoryNotification,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "cancel_mailbox_inventory",
            })
        })
    }

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

    /// Reads metadata staged for a reserved thread without persisting it.
    fn read_pending_thread_metadata(
        &self,
        _thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<ThreadMetadataPatch>> {
        Box::pin(async { Ok(None) })
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
        _thread_ids: Vec<ThreadId>,
    ) -> ThreadStoreFuture<'_, ThreadWriterReservation> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "reserve_thread_writers",
            })
        })
    }

    /// Appends raw rollout items to a live thread.
    ///
    /// Implementations should apply the shared rollout persistence policy before writing durable
    /// replay history and before updating any implementation-owned projections.
    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()>;

    /// Appends a completion batch once and acknowledges its canonical write barrier.
    ///
    /// Persist session metadata first and serialize the entire batch against other writes.
    /// The batch is prefix-committable, not atomic. An error or lost acknowledgement after
    /// dispatch means commit unknown; implementations must not queue failed items for retry.
    /// Rebuildable projection failures after acknowledgement must not become append failures.
    /// An empty batch still acknowledges a barrier over existing canonical history; it is not
    /// a no-op and must not be used to clear an unknown commit on the same live writer.
    fn append_completion_items_and_flush(
        &self,
        params: AppendThreadItemsParams,
    ) -> ThreadStoreFuture<'_, ()>;

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
    /// Standard persistence must complete before returning. Contexts that allow background
    /// persistence may complete asynchronously when the implementation enqueues the checkpoint
    /// before returning, fences it with subsequent flush or shutdown operations, and surfaces
    /// failures through those operations.
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

    /// Loads persisted history for resume, fork, and memory jobs.
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory>;

    /// Loads full raw artifact history, retaining original lineage and rollback coordinates.
    ///
    /// Unlike model-context reads, this cannot discard records before a checkpoint. This
    /// is evidence for validation and replay only, never acknowledgement of an uncertain write.
    fn load_canonical_artifact_segments(
        &self,
        _params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, crate::StoredCanonicalArtifactSegments> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "load_canonical_artifact_segments",
            })
        })
    }

    /// Locates a trusted completion-context item by its stable reserved identity.
    ///
    /// This lookup spans canonical history even when paginated model-context reads use a bounded
    /// suffix, and returns only the matching artifact. Preserve each source's exact rollback
    /// coordinates and original metadata adjacency. Visibility is not a durability receipt.
    fn load_sub_agent_completion_context_item(
        &self,
        params: LoadSubAgentCompletionContextItemParams,
    ) -> ThreadStoreFuture<'_, Option<ResponseItem>>;

    /// Locates a canonical completion presentation and the queried turn's lifecycle.
    ///
    /// This lookup spans canonical history, including frozen inherited prefixes. A matching
    /// identity in a different turn is a conflict. Visibility does not acknowledge durability
    /// and must never clear a live writer's commit-unknown quarantine.
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

    /// Loads a complete copied-fork source addressed by rollout path.
    ///
    /// Unlike [`ThreadStore::prepare_fork`], the returned source owns its inherited records and
    /// does not retain references to or mutate source-store rollouts.
    fn load_fork_source_by_rollout_path(
        &self,
        _params: LoadForkSourceByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredForkSource> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "load_fork_source_by_rollout_path",
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

    /// Whether this store can persist and discover thread-owned attachments.
    fn supports_thread_attachments(&self) -> bool {
        false
    }

    /// Copies current attachment membership into a newly persisted fork.
    ///
    /// Copies must be atomic and use new attachment IDs. The destination must be empty;
    /// subsequent membership changes on either thread must remain independent.
    fn copy_thread_attachments(
        &self,
        _source_thread_id: ThreadId,
        _destination_thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "copy_thread_attachments",
            })
        })
    }

    /// Attaches an attachment, returning an existing attachment for repeated requests.
    fn add_thread_attachment(
        &self,
        _params: AddThreadAttachmentParams,
    ) -> ThreadStoreFuture<'_, AddThreadAttachmentOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachment/add",
            })
        })
    }

    /// Lists attachments belonging to one persisted thread.
    fn list_thread_attachments(
        &self,
        _params: ListThreadAttachmentsParams,
    ) -> ThreadStoreFuture<'_, ThreadAttachmentPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachment/list",
            })
        })
    }

    /// Removes an attachment and reports whether it existed.
    fn remove_thread_attachment(
        &self,
        _params: RemoveThreadAttachmentParams,
    ) -> ThreadStoreFuture<'_, RemoveThreadAttachmentOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachment/remove",
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
    /// Success includes cleanup of associated persisted state; callers must not repeat it.
    /// Durable agent graph and alias evidence is retained without reviving deleted identities.
    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Deletes threads and their associated persisted state in order, treating already-missing
    /// members as deleted.
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
