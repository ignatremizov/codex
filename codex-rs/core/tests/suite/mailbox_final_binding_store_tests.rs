//! Transparent test store: delay only the selected sender's canonical proof reads.

use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_thread_store as store;
use std::any::Any;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProofGateState {
    Blocked,
    Released,
}

pub(super) struct DelayedProofStore {
    pub(super) inner: Arc<dyn store::ThreadStore>,
    pub(super) blocked_sender: Mutex<Option<ThreadId>>,
    pub(super) entered: Notify,
    pub(super) release: watch::Receiver<ProofGateState>,
}

macro_rules! forward_store {
    ($(fn $name:ident $(<$lifetime:lifetime>)? (& $( $self_lifetime:lifetime)? self $(, $arg:ident: $arg_ty:ty)* $(,)?) -> $result:ty;)*) => {
        $(fn $name $(<$lifetime>)? (& $( $self_lifetime)? self $(, $arg: $arg_ty)*) -> $result {
            self.inner.$name($($arg),*)
        })*
    };
}

impl store::ThreadStore for DelayedProofStore {
    fn as_any(&self) -> &dyn Any {
        self.inner.as_any()
    }

    fn load_mailbox_canonical_history(
        &self,
        receiver_thread_id: ThreadId,
    ) -> store::ThreadStoreFuture<'_, Vec<RolloutItem>> {
        Box::pin(async move {
            let blocked =
                *self.blocked_sender.lock().expect("proof gate sender") == Some(receiver_thread_id);
            if blocked {
                let mut release = self.release.clone();
                self.entered.notify_one();
                release
                    .wait_for(|state| *state == ProofGateState::Released)
                    .await
                    .map_err(|error| store::ThreadStoreError::Internal {
                        message: format!("proof gate closed: {error}"),
                    })?;
            }
            self.inner
                .load_mailbox_canonical_history(receiver_thread_id)
                .await
        })
    }

    // Forward optional capabilities as well as required methods so this gate cannot
    // make an unsupported fallback, different history mode, or missing authority pass.
    forward_store! {
        fn accept_mailbox_input( &self, params: store::AcceptMailboxInputParams, ) -> store::ThreadStoreFuture<'_, store::StoredMailboxInput>;
        fn accept_mailbox_input_with_authority( &self, params: store::AcceptMailboxInputParams, authority: store::MailboxFinalSubscriptionAuthority, ) -> store::ThreadStoreFuture<'_, store::StoredMailboxInput>;
        fn lookup_active_mailbox_final_subscription( &self, receiver_thread_id: codex_protocol::ThreadId, sender_thread_id: codex_protocol::ThreadId, ) -> store::ThreadStoreFuture<'_, Option<store::MailboxFinalSubscription>>;
        fn lookup_mailbox_final_subscription( &self, receiver_thread_id: codex_protocol::ThreadId, message_id: String, ) -> store::ThreadStoreFuture<'_, Option<store::MailboxFinalSubscription>>;
        fn read_active_mailbox_final_subscriptions_for_thread( &self, thread_id: codex_protocol::ThreadId, ) -> store::ThreadStoreFuture<'_, Vec<store::MailboxFinalSubscription>>;
        fn supersede_mailbox_final_subscription( &self, receiver_thread_id: codex_protocol::ThreadId, sender_thread_id: codex_protocol::ThreadId, ) -> store::ThreadStoreFuture<'_, ()>;
        fn supersede_mailbox_final_subscription_message( &self, receiver_thread_id: codex_protocol::ThreadId, message_id: String, ) -> store::ThreadStoreFuture<'_, ()>;
        fn supersede_mailbox_final_subscriptions_for_threads( &self, thread_ids: Vec<codex_protocol::ThreadId>, ) -> store::ThreadStoreFuture<'_, ()>;
        fn acknowledge_mailbox_final_subscription_delivery( &self, receiver_thread_id: codex_protocol::ThreadId, message_id: String, turn_id: String, ) -> store::ThreadStoreFuture<'_, ()>;
        fn lookup_mailbox_input<'a>( &'a self, receiver_thread_id: codex_protocol::ThreadId, submission_key: &'a str, ) -> store::ThreadStoreFuture<'a, Option<store::StoredMailboxInput>>;
        fn lookup_mailbox_claim( &self, invocation: store::MailboxInvocation, ) -> store::ThreadStoreFuture<'_, Option<store::MailboxClaim>>;
        fn claim_mailbox_input( &self, params: store::ClaimMailboxInputParams, ) -> store::ThreadStoreFuture<'_, store::MailboxClaim>;
        fn recover_mailbox_delivery( &self, params: store::ClaimMailboxInputParams, ) -> store::ThreadStoreFuture<'_, store::RecoveredMailboxClaim>;
        fn reject_mailbox_input( &self, params: store::RejectMailboxInputParams, ) -> store::ThreadStoreFuture<'_, store::StoredMailboxInput>;
        fn reconcile_mailbox_delivery( &self, params: store::ReconcileMailboxDeliveryParams, ) -> store::ThreadStoreFuture<'_, store::MailboxClaim>;
        fn read_mailbox_inventory( &self, receiver_thread_id: codex_protocol::ThreadId, ) -> store::ThreadStoreFuture<'_, store::MailboxInventory>;
        fn has_pending_mailbox_inventory( &self, notification: store::MailboxInventoryNotification, ) -> store::ThreadStoreFuture<'_, bool>;
        fn prepare_mailbox_inventory( &self, receiver_thread_id: codex_protocol::ThreadId, ) -> store::ThreadStoreFuture<'_, Option<store::MailboxInventoryNotification>>;
        fn recover_mailbox_inventory( &self, notification: store::MailboxInventoryNotification, ) -> store::ThreadStoreFuture<'_, store::MailboxInventoryRecovery>;
        fn reconcile_mailbox_inventory( &self, notification: store::MailboxInventoryNotification, ) -> store::ThreadStoreFuture<'_, store::MailboxInventoryAcknowledgement>;
        fn cancel_mailbox_inventory( &self, notification: store::MailboxInventoryNotification, ) -> store::ThreadStoreFuture<'_, ()>;
        fn default_history_mode(&self) -> ThreadHistoryMode;
        fn create_thread(&self, params: store::CreateThreadParams) -> store::ThreadStoreFuture<'_, ()>;
        fn stage_pending_thread_metadata( &self, thread_id: ThreadId, patch: store::ThreadMetadataPatch, ) -> store::ThreadStoreFuture<'_, ()>;
        fn read_pending_thread_metadata( &self, thread_id: ThreadId, ) -> store::ThreadStoreFuture<'_, Option<store::ThreadMetadataPatch>>;
        fn remove_pending_thread_metadata(&self, thread_id: ThreadId) -> store::ThreadStoreFuture<'_, ()>;
        fn resume_thread(&self, params: store::ResumeThreadParams) -> store::ThreadStoreFuture<'_, ()>;
        fn reserve_thread_writers( &self, thread_ids: Vec<ThreadId>, ) -> store::ThreadStoreFuture<'_, store::ThreadWriterReservation>;
        fn append_items(&self, params: store::AppendThreadItemsParams) -> store::ThreadStoreFuture<'_, ()>;
        fn record_thread_metadata( &self, params: store::UpdateThreadMetadataParams, ) -> store::ThreadStoreFuture<'_, ()>;
        fn append_completion_items_and_flush( &self, params: store::AppendThreadItemsParams, ) -> store::ThreadStoreFuture<'_, ()>;
        fn append_items_and_flush(&self, params: store::AppendThreadItemsParams) -> store::ThreadStoreFuture<'_, ()>;
        fn persist_thread( &self, thread_id: ThreadId, context: store::PersistContext, ) -> store::ThreadStoreFuture<'_, ()>;
        fn flush_thread(&self, thread_id: ThreadId) -> store::ThreadStoreFuture<'_, ()>;
        fn shutdown_thread(&self, thread_id: ThreadId) -> store::ThreadStoreFuture<'_, ()>;
        fn discard_thread(&self, thread_id: ThreadId) -> store::ThreadStoreFuture<'_, ()>;
        fn load_history( &self, params: store::LoadThreadHistoryParams, ) -> store::ThreadStoreFuture<'_, store::StoredThreadHistory>;
        fn load_canonical_artifact_segments( &self, params: store::LoadThreadHistoryParams, ) -> store::ThreadStoreFuture<'_, store::StoredCanonicalArtifactSegments>;
        fn load_sub_agent_completion_context_item( &self, params: store::LoadSubAgentCompletionContextItemParams, ) -> store::ThreadStoreFuture<'_, Option<ResponseItem>>;
        fn load_sub_agent_completion_presentation( &self, params: store::LoadSubAgentCompletionPresentationParams, ) -> store::ThreadStoreFuture<'_, store::StoredSubAgentCompletionPresentation>;
        fn load_latest_model_context( &self, params: store::LoadThreadHistoryParams, ) -> store::ThreadStoreFuture<'_, store::StoredModelContext>;
        fn prepare_fork(&self, params: store::PrepareForkParams) -> store::ThreadStoreFuture<'_, store::PreparedFork>;
        fn load_fork_source_by_rollout_path( &self, params: store::LoadForkSourceByRolloutPathParams, ) -> store::ThreadStoreFuture<'_, store::StoredForkSource>;
        fn revert_thread(&self, params: store::RevertThreadParams) -> store::ThreadStoreFuture<'_, ()>;
        fn read_thread(&self, params: store::ReadThreadParams) -> store::ThreadStoreFuture<'_, store::StoredThread>;
        fn read_thread_by_rollout_path( &self, params: store::ReadThreadByRolloutPathParams, ) -> store::ThreadStoreFuture<'_, store::StoredThread>;
        fn list_threads(&self, params: store::ListThreadsParams) -> store::ThreadStoreFuture<'_, store::ThreadPage>;
        fn supports_thread_sections(&self) -> bool;
        fn list_thread_sections( &self, params: store::ListThreadSectionsParams, ) -> store::ThreadStoreFuture<'_, store::StoredThreadSectionsPage>;
        fn create_thread_section( &self, params: store::CreateThreadSectionParams, ) -> store::ThreadStoreFuture<'_, store::StoredThreadSection>;
        fn rename_thread_section( &self, params: store::RenameThreadSectionParams, ) -> store::ThreadStoreFuture<'_, Option<store::StoredThreadSection>>;
        fn delete_thread_section( &self, params: store::DeleteThreadSectionParams, ) -> store::ThreadStoreFuture<'_, bool>;
        fn supports_thread_attachments(&self) -> bool;
        fn copy_thread_attachments( &self, source_thread_id: ThreadId, destination_thread_id: ThreadId, ) -> store::ThreadStoreFuture<'_, ()>;
        fn add_thread_attachment( &self, params: store::AddThreadAttachmentParams, ) -> store::ThreadStoreFuture<'_, store::AddThreadAttachmentOutcome>;
        fn list_thread_attachments( &self, params: store::ListThreadAttachmentsParams, ) -> store::ThreadStoreFuture<'_, store::ThreadAttachmentPage>;
        fn list_thread_attachment_threads( &self, params: store::ListThreadAttachmentThreadsParams, ) -> store::ThreadStoreFuture<'_, store::ThreadAttachmentOwnerPage>;
        fn remove_thread_attachment( &self, params: store::RemoveThreadAttachmentParams, ) -> store::ThreadStoreFuture<'_, store::RemoveThreadAttachmentOutcome>;
        fn supports_projects(&self) -> bool;
        fn list_projects( &self, params: store::ListProjectsParams, ) -> store::ThreadStoreFuture<'_, store::StoredProjectsPage>;
        fn read_project(&self, project_id: String) -> store::ThreadStoreFuture<'_, Option<store::StoredProject>>;
        fn create_project( &self, params: store::CreateProjectParams, ) -> store::ThreadStoreFuture<'_, store::CreatedProject>;
        fn update_project( &self, params: store::UpdateProjectParams, ) -> store::ThreadStoreFuture<'_, Option<store::UpdatedProject>>;
        fn move_project( &self, params: store::MoveProjectParams, ) -> store::ThreadStoreFuture<'_, Option<store::ProjectMoveOutcome>>;
        fn delete_project(&self, project_id: String) -> store::ThreadStoreFuture<'_, Option<store::DeletedProject>>;
        fn supports_paginated_history_lists(&self) -> bool;
        fn search_threads( &self, params: store::SearchThreadsParams, ) -> store::ThreadStoreFuture<'_, store::ThreadSearchPage>;
        fn search_thread_occurrences( &self, params: store::SearchThreadOccurrencesParams, ) -> store::ThreadStoreFuture<'_, store::ThreadOccurrenceSearchPage>;
        fn list_turns(&self, params: store::ListTurnsParams) -> store::ThreadStoreFuture<'_, store::TurnPage>;
        fn list_items(&self, params: store::ListItemsParams) -> store::ThreadStoreFuture<'_, store::ItemPage>;
        fn list_timeline( &self, params: store::ListTimelineParams, ) -> store::ThreadStoreFuture<'_, store::TimelinePage>;
        fn update_thread_metadata( &self, params: store::UpdateThreadMetadataParams, ) -> store::ThreadStoreFuture<'_, Option<store::StoredThread>>;
        fn move_thread_to_section( &self, params: store::MoveThreadToSectionParams, ) -> store::ThreadStoreFuture<'_, ()>;
        fn archive_thread(&self, params: store::ArchiveThreadParams) -> store::ThreadStoreFuture<'_, ()>;
        fn archive_threads( &self, params: store::ArchiveThreadsParams, ) -> store::ThreadStoreFuture<'_, Vec<ThreadId>>;
        fn unarchive_thread(&self, params: store::ArchiveThreadParams) -> store::ThreadStoreFuture<'_, store::StoredThread>;
        fn delete_thread(&self, params: store::DeleteThreadParams) -> store::ThreadStoreFuture<'_, ()>;
        fn delete_threads(&self, params: store::DeleteThreadsParams) -> store::ThreadStoreFuture<'_, ()>;
    }
}
