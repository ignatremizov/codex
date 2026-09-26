use super::*;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnAbortReason;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::DeleteThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::LiveThread;
use codex_thread_store::LoadSubAgentCompletionContextItemParams;
use codex_thread_store::LoadSubAgentCompletionPresentationParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::PersistContext;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::StoredSubAgentCompletionPresentation;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredThreadHistory;
use codex_thread_store::ThreadPage;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreFuture;
use codex_thread_store::UpdateThreadMetadataParams;
use pretty_assertions::assert_eq;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppendGate {
    BeforeCommit,
    AfterCommit,
    AmbiguousFailure,
    BeforeFlush,
    FlushFailure,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    Retry,
    AbortAll,
    AbortOne,
    AbortAllPreCancelled,
    AbortOnePreCancelled,
}

struct GatedAppendStore {
    inner: InMemoryThreadStore,
    phase: AppendGate,
    release: AsyncMutex<Option<oneshot::Receiver<()>>>,
    appends: AtomicUsize,
    gate_polls: AtomicUsize,
    // One admitted asynchronous writer; there is no guarded data to borrow.
    writer: Semaphore,
}

struct PendingTask {
    recovery: Recovery,
}

impl SessionTask for PendingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.mcp_persistence_test"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        turn_context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        cancellation_token.cancelled().await;
        // Token cancellation need not stop the task's polling. Model a task
        // still awaiting the publication so abort must stop its handle before
        // acquiring the same pending-publication mutex.
        if matches!(
            self.recovery,
            Recovery::AbortAllPreCancelled | Recovery::AbortOnePreCancelled
        ) {
            session
                .record_mcp_use_items(turn_context.model_info(), Vec::new())
                .await;
        }
        Err(CodexErr::TurnAborted)
    }
}

macro_rules! delegate_store_methods {
    ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
        $(fn $name(&self, $param: $params) -> ThreadStoreFuture<'_, $result> {
            ThreadStore::$name(&self.inner, $param)
        })*
    };
}

impl ThreadStore for GatedAppendStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    delegate_store_methods! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn load_sub_agent_completion_context_item(
            params: LoadSubAgentCompletionContextItemParams
        ) -> Option<codex_protocol::models::ResponseItem>;
        fn load_sub_agent_completion_presentation(
            params: LoadSubAgentCompletionPresentationParams
        ) -> StoredSubAgentCompletionPresentation;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn update_thread_metadata(params: UpdateThreadMetadataParams) -> Option<StoredThread>;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }

    fn append_completion_items_and_flush(
        &self,
        params: AppendThreadItemsParams,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.append_completion_items_and_flush(params)
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let release = if matches!(
                self.phase,
                AppendGate::BeforeFlush | AppendGate::FlushFailure
            ) {
                self.release.lock().await.take()
            } else {
                None
            };
            if let Some(mut release) = release {
                std::future::poll_fn(|context| {
                    self.gate_polls.fetch_add(1, Ordering::SeqCst);
                    Pin::new(&mut release).poll(context)
                })
                .await
                .expect("release flush");
                if self.phase == AppendGate::FlushFailure {
                    return Err(codex_thread_store::ThreadStoreError::Internal {
                        message: "writer flush failed after commit".to_string(),
                    });
                }
            }
            self.inner.flush_thread(thread_id).await
        })
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _writer = self.writer.acquire().await.expect("test writer stays open");
            if !params.items.iter().any(|item| {
                matches!(item,
                RolloutItem::ResponseItem(item)
                    if McpServerUseInstructions::matches_response_item(&item.item))
            }) {
                return self.inner.append_items(params).await;
            }
            self.appends.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.phase,
                AppendGate::BeforeFlush | AppendGate::FlushFailure
            ) {
                return self.inner.append_items(params).await;
            }
            let mut release = self.release.lock().await.take().expect("one append");
            let released = std::future::poll_fn(|context| {
                self.gate_polls.fetch_add(1, Ordering::SeqCst);
                Pin::new(&mut release).poll(context)
            });
            match self.phase {
                AppendGate::BeforeCommit => {
                    released.await.expect("release append");
                    self.inner.append_items(params).await
                }
                AppendGate::AfterCommit | AppendGate::AmbiguousFailure => {
                    self.inner.append_items(params).await?;
                    released.await.expect("release append");
                    if self.phase == AppendGate::AmbiguousFailure {
                        Err(codex_thread_store::ThreadStoreError::Internal {
                            message: "metadata projection failed after commit".to_string(),
                        })
                    } else {
                        Ok(())
                    }
                }
                AppendGate::BeforeFlush | AppendGate::FlushFailure => {
                    unreachable!("flush gates do not wait during append")
                }
            }
        })
    }
}

#[path = "durable_context_tests.rs"]
mod durable_publication_tests;

#[tokio::test]
async fn cancellation_resumes_the_same_append_before_or_after_store_commit() {
    for (phase, recovery) in [AppendGate::BeforeCommit, AppendGate::AfterCommit]
        .into_iter()
        .flat_map(|phase| {
            [
                Recovery::Retry,
                Recovery::AbortAll,
                Recovery::AbortOne,
                Recovery::AbortAllPreCancelled,
                Recovery::AbortOnePreCancelled,
            ]
            .map(|recovery| (phase, recovery))
        })
    {
        let (mut session, turn) = make_session_and_context().await;
        let (release, gate) = oneshot::channel();
        let store = Arc::new(GatedAppendStore {
            inner: InMemoryThreadStore::default(),
            phase,
            release: AsyncMutex::new(Some(gate)),
            appends: AtomicUsize::new(0),
            gate_polls: AtomicUsize::new(0),
            writer: Semaphore::new(/*permits*/ 1),
        });
        let thread_store: Arc<dyn ThreadStore> = store.clone();
        let config = session.get_config().await;
        session.services.live_thread = Some(
            LiveThread::create(
                thread_store,
                CreateThreadParams {
                    session_id: session.session_id(),
                    thread_id: session.thread_id,
                    extra_config: None,
                    forked_from_id: None,
                    parent_thread_id: None,
                    source: SessionSource::Exec,
                    thread_source: None,
                    originator: "mcp-use-test".to_string(),
                    base_instructions: BaseInstructions::default(),
                    dynamic_tools: Vec::new(),
                    selected_capability_roots: Vec::new(),
                    multi_agent_version: None,
                    history_mode: ThreadHistoryMode::Legacy,
                    subagent_history_start_ordinal: None,
                    history_base: None,
                    initial_window_id: uuid::Uuid::now_v7().to_string(),
                    runtime_workspace_roots: None,
                    metadata: ThreadPersistenceMetadata {
                        cwd: Some(config.cwd.to_path_buf()),
                        model_provider: config.model_provider_id.clone(),
                        memory_mode: ThreadMemoryMode::Disabled,
                    },
                },
            )
            .await
            .expect("create persistence"),
        );
        let session = Arc::new(session);
        let step = StepContext::for_test(Arc::new(turn));
        session.activate_mcp_server("unavailable".to_string()).await;
        if recovery != Recovery::Retry {
            session
                .start_task(Arc::clone(&step.turn), Vec::new(), PendingTask { recovery })
                .await;
        }
        let mut recording = Box::pin(session.record_queued_mcp_use(&step));
        assert!(futures::poll!(recording.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.gate_polls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("independent publication reaches append gate");
        assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        drop(recording);
        let expected = vec![render_inventory("unavailable", &[])];
        let unpublished = session
            .clone_history()
            .await
            .annotated_items()
            .iter()
            .filter(|item| McpServerUseInstructions::matches_response_item(&item.item))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(unpublished, Vec::<ResponseItemEnvelope>::new());
        assert_eq!(
            *session.mcp_prompt.first_turn_servers.lock().await,
            vec!["unavailable"]
        );
        let stored = store
            .inner
            .load_history(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("history at cancellation")
            .items;
        let stored = stored
            .into_iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(item)
                    if McpServerUseInstructions::matches_response_item(&item.item) =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            stored,
            if phase == AppendGate::AfterCommit {
                expected.clone()
            } else {
                Vec::new()
            }
        );

        // Both retry and real abort cleanup wait for the independently driven worker.
        if matches!(
            recovery,
            Recovery::AbortAllPreCancelled | Recovery::AbortOnePreCancelled
        ) {
            session
                .active_turn
                .lock()
                .await
                .as_ref()
                .expect("active turn")
                .task
                .as_ref()
                .expect("running task")
                .cancellation_token
                .cancel();
            tokio::task::yield_now().await;
        }
        let mut recovering = Box::pin(async {
            match recovery {
                Recovery::Retry => session.record_queued_mcp_use(&step).await,
                Recovery::AbortAll | Recovery::AbortAllPreCancelled => {
                    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
                }
                Recovery::AbortOne | Recovery::AbortOnePreCancelled => {
                    assert!(
                        session
                            .abort_turn_if_active(&step.turn.sub_id, TurnAbortReason::Interrupted,)
                            .await
                    );
                }
            }
        });
        assert!(futures::poll!(recovering.as_mut()).is_pending());
        assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        release
            .send(())
            .expect("retained append still owns its gate");
        tokio::time::timeout(Duration::from_secs(5), recovering)
            .await
            .expect("recovery must finish without writer-lock deadlock");
        // Abort retains the admitted name until the next real turn, which then
        // deduplicates against the fully completed publication.
        session.record_queued_mcp_use(&step).await;
        assert!(
            session
                .mcp_prompt
                .first_turn_servers
                .lock()
                .await
                .is_empty()
        );
        session
            .check_history_publication()
            .expect("publication succeeded");
        let live = session
            .clone_history()
            .await
            .annotated_items()
            .iter()
            .filter(|item| McpServerUseInstructions::matches_response_item(&item.item))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(live, expected);
        let stored = store
            .inner
            .load_history(LoadThreadHistoryParams {
                thread_id: session.thread_id,
                include_archived: false,
            })
            .await
            .expect("history after retry")
            .items;
        let stored = stored
            .into_iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(item)
                    if McpServerUseInstructions::matches_response_item(&item.item) =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(stored, expected);
        assert_eq!(store.appends.load(Ordering::SeqCst), 1);
        assert!(session.active_turn.lock().await.is_none());
        assert!(!session.input_queue.has_queued_turn_trigger().await);
    }
}
