use super::*;
use crate::session::tests::make_session_and_context;
use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::*;
use pretty_assertions::assert_eq;
use std::any::Any;
use std::future::Future;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

/// Gates checked cancellation before Core can clear its active placeholder.
struct CancellationGateStore {
    inner: Arc<dyn ThreadStore>,
    gate: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    inventory_reads: AtomicUsize,
}

macro_rules! forward_store {
    ($(fn $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)*) => {
        $(fn $method(&self, $($arg: $ty),*) -> ThreadStoreFuture<'_, $result> {
            self.inner.$method($($arg),*)
        })*
    };
}

impl ThreadStore for CancellationGateStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    forward_store! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> ();
        fn reserve_thread_writers(thread_ids: Vec<ThreadId>) -> ThreadWriterReservation;
        fn append_items(params: AppendThreadItemsParams) -> ();
        fn persist_thread(thread_id: ThreadId, context: PersistContext) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn load_sub_agent_completion_context_item(params: LoadSubAgentCompletionContextItemParams) -> Option<ResponseItem>;
        fn load_sub_agent_completion_presentation(params: LoadSubAgentCompletionPresentationParams) -> StoredSubAgentCompletionPresentation;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn update_thread_metadata(params: UpdateThreadMetadataParams) -> Option<StoredThread>;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
        fn has_pending_mailbox_inventory(notification: MailboxInventoryNotification) -> bool;
        fn recover_mailbox_inventory(notification: MailboxInventoryNotification) -> MailboxInventoryRecovery;
    }

    fn read_mailbox_inventory(
        &self,
        receiver: ThreadId,
    ) -> ThreadStoreFuture<'_, MailboxInventory> {
        self.inventory_reads.fetch_add(/*val*/ 1, Ordering::SeqCst);
        self.inner.read_mailbox_inventory(receiver)
    }

    fn cancel_mailbox_inventory(
        &self,
        notification: MailboxInventoryNotification,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.inner.cancel_mailbox_inventory(notification).await?;
            let gate = self.gate.lock().await.take();
            if let Some((reached, release)) = gate {
                let _ = reached.send(());
                let _ = release.await;
            }
            Ok(())
        })
    }
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "block placeholder cleanup while explicitly polling the admission to check read ordering"
)]
async fn arrival_after_obsolete_check_before_placeholder_cleanup_is_not_lost() -> anyhow::Result<()>
{
    let (mut session, _) = make_session_and_context().await;
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::default());
    session.services.live_thread = Some(
        LiveThread::create(
            Arc::clone(&store),
            CreateThreadParams {
                session_id: session.session_id(),
                thread_id: session.thread_id,
                extra_config: None,
                forked_from_id: None,
                parent_thread_id: None,
                source: SessionSource::Exec,
                thread_source: None,
                originator: "inventory-race-test".to_string(),
                base_instructions: BaseInstructions::default(),
                dynamic_tools: Vec::new(),
                selected_capability_roots: Vec::new(),
                multi_agent_version: Some(MultiAgentVersion::V1),
                history_mode: ThreadHistoryMode::Legacy,
                subagent_history_start_ordinal: None,
                history_base: None,
                initial_window_id: uuid::Uuid::now_v7().to_string(),
                metadata: ThreadPersistenceMetadata {
                    cwd: None,
                    model_provider: "test".to_string(),
                    memory_mode: ThreadMemoryMode::Disabled,
                },
            },
        )
        .await?,
    );
    let receiver = session.thread_id;
    let accept = |key: &str| AcceptMailboxInputParams {
        receiver_thread_id: receiver,
        submission_key: key.to_string(),
        payload: MailboxPayload::User {
            input: vec![UserInput::Text {
                text: format!("PRIVATE_{key}_PAYLOAD"),
                text_elements: Vec::new(),
            }],
            client_id: None,
        },
    };
    let old = store.accept_mailbox_input(accept("old")).await?;
    let notification = store
        .prepare_mailbox_inventory(receiver)
        .await?
        .context("original inventory missing")?;
    store
        .reject_mailbox_input(RejectMailboxInputParams {
            receiver_thread_id: receiver,
            message_id: old.id,
            reason: "old mail retired before notification".to_string(),
        })
        .await?;
    let (reached, reached_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let gated_store = Arc::new(CancellationGateStore {
        inner: Arc::clone(&store),
        gate: Mutex::new(Some((reached, release_rx))),
        inventory_reads: AtomicUsize::new(/*v*/ 0),
    });
    session.services.thread_store = gated_store.clone();
    let session = Arc::new(session);
    let mut admission =
        Box::pin(session.try_start_automatic_idle_with_lease(
            AutomaticIdleAdmission::Inventory(notification),
            (),
        ));
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), async {
        tokio::select! {
            result = &mut admission => {
                anyhow::bail!("admission completed before cancellation gate: {result:?}");
            }
            result = reached_rx => result?,
        }
        anyhow::Ok(())
    })
    .await??;
    let placeholder = session.active_turn.lock().await;
    assert!(placeholder.is_some());
    assert!(
        store
            .read_mailbox_inventory(receiver)
            .await?
            .active_notification
            .is_none()
    );
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("cancellation gate closed"))?;

    // Finish cancellation, then drive the admission until it must wait for the
    // held placeholder mutex. InMemoryThreadStore has no contended locks here:
    // the old ordering would already have called read_mailbox_inventory and
    // snapshotted the empty inbox. The fixed ordering cannot call it yet.
    // Disable cooperative-budget yields so Pending identifies the held mutex,
    // not an incidental runtime scheduling boundary.
    let polled = {
        let mut admission_poll = Box::pin(tokio::task::unconstrained(admission.as_mut()));
        std::future::poll_fn(|cx| Poll::Ready(admission_poll.as_mut().poll(cx))).await
    };
    assert!(polled.is_pending());
    assert_eq!(gated_store.inventory_reads.load(Ordering::SeqCst), 0);

    let new = store.accept_mailbox_input(accept("new")).await?;
    session.notify_mailbox_activity();
    drop(placeholder);
    let outcome = tokio::time::timeout(Duration::from_secs(/*secs*/ 15), admission)
        .await?
        .map_err(|error| anyhow::anyhow!("admission rejected: {:?}", error.reason()))?;
    assert_eq!(outcome, MailboxInventoryAdmission::Retry);
    assert_eq!(gated_store.inventory_reads.load(Ordering::SeqCst), 1);
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        store
            .read_mailbox_inventory(receiver)
            .await?
            .pending_senders,
        vec![MailboxSenderInventory {
            sender: MailboxSender::User,
            count: 1,
            max_acceptance_sequence: new.acceptance_sequence,
        }],
    );
    assert!(!session.input_queue.has_pending_mailbox_items().await);
    Ok(())
}
