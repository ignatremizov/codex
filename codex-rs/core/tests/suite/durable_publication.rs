//! Ambiguous canonical commit must stop retries and later manual compaction.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::DeleteThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::PersistContext;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredThreadHistory;
use codex_thread_store::ThreadPage;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreFuture;
use codex_thread_store::UpdateThreadMetadataParams;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Default)]
struct AmbiguousAssistantStore {
    inner: InMemoryThreadStore,
    failed_appends: AtomicUsize,
    settings_failure: bool,
}

macro_rules! delegate_store_methods {
    ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
        $(fn $name(&self, $param: $params) -> ThreadStoreFuture<'_, $result> {
            ThreadStore::$name(&self.inner, $param)
        })*
    };
}

impl ThreadStore for AmbiguousAssistantStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    delegate_store_methods! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn update_thread_metadata(params: UpdateThreadMetadataParams) -> Option<StoredThread>;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let fails = params.items.iter().any(|item| {
                if self.settings_failure {
                    matches!(item, RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_)))
                } else {
                    matches!(item, RolloutItem::ResponseItem(envelope)
                        if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "assistant"))
                }
            });
            self.inner.append_items(params).await?;
            if fails {
                self.failed_appends.fetch_add(1, Ordering::SeqCst);
                return Err(ThreadStoreError::Internal {
                    message: "metadata projection failed after canonical commit".to_string(),
                });
            }
            Ok(())
        })
    }
}

#[test_case(ThreadHistoryMode::Legacy, false, false; "legacy_local_retry")]
#[test_case(ThreadHistoryMode::Legacy, true, false; "legacy_remote_retry")]
#[test_case(ThreadHistoryMode::Paginated, false, false; "paginated_local_retry")]
#[test_case(ThreadHistoryMode::Paginated, true, false; "paginated_remote_retry")]
#[test_case(ThreadHistoryMode::Legacy, false, true; "legacy_completed_stream")]
#[test_case(ThreadHistoryMode::Paginated, true, true; "paginated_completed_stream")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_publication_prevents_stream_retry_and_manual_compaction(
    history_mode: ThreadHistoryMode,
    remote_compaction: bool,
    completed_stream: bool,
) -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let mut events = vec![
        responses::ev_response_created("first"),
        responses::ev_assistant_message("canonical-before-failure", "accepted before disconnect"),
    ];
    if completed_stream {
        events.push(responses::ev_completed("first"));
    }
    // An incomplete stream ordinarily retries; a completed one must still report failure.
    let response = responses::mount_sse_once(&server, responses::sse(events)).await;
    let store = Arc::new(AmbiguousAssistantStore::default());
    let test = test_codex()
        .with_thread_store(store.clone())
        .with_history_mode(history_mode)
        .with_config(move |config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config
                .features
                .set_enabled(Feature::RemoteCompaction, remote_compaction)
                .expect("compaction mode");
            config
                .features
                .set_enabled(Feature::RemoteCompactionV2, remote_compaction)
                .expect("compaction v2");
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "answer once".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    assert!(
        matches!(error, EventMsg::Error(error) if error.message.contains("canonical reload required"))
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(store.failed_appends.load(Ordering::SeqCst), 1);
    assert_eq!(response.requests().len(), 1);
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("captured requests")
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        1
    );

    test.codex.submit(Op::Compact).await?;
    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    assert!(
        matches!(error, EventMsg::Error(error) if error.message.contains("canonical reload required"))
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(response.requests().len(), 1);
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("captured requests")
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        1
    );
    assert_eq!(store.failed_appends.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn ambiguous_settings_append_rejects_admission_and_does_not_sample() -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(&server, responses::sse_completed("unexpected")).await;
    let store = Arc::new(AmbiguousAssistantStore {
        settings_failure: true,
        ..Default::default()
    });
    let test = test_codex()
        .with_thread_store(store.clone())
        .build_with_auto_env(&server)
        .await?;
    let result = test
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "must not sample".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                service_tier: Some(Some("fast".to_string())),
                ..Default::default()
            }),
        )
        .await;
    assert!(
        matches!(result, Err(error) if error.to_string().contains("canonical reload required"))
    );
    assert_eq!(store.failed_appends.load(Ordering::SeqCst), 1);
    assert!(response.requests().is_empty());
    let retry = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "still must not sample".to_string(),
            text_elements: Vec::new(),
        }]))
        .await;
    assert!(retry.is_err());
    assert_eq!(store.failed_appends.load(Ordering::SeqCst), 1);
    assert!(response.requests().is_empty());
    Ok(())
}
