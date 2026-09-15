use super::*;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::*;
use pretty_assertions::assert_eq;
use std::any::Any;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Clone, Copy)]
enum AcceptanceFault {
    Unsupported,
    AcknowledgementLost,
}

/// Faults the acceptance acknowledgement and subsequent reads, not canonical publication.
struct RecoveryFaultStore {
    inner: InMemoryThreadStore,
    fault: AcceptanceFault,
    acceptance_attempts: AtomicUsize,
    lookup_attempts: AtomicUsize,
    lookup_started: Notify,
}

macro_rules! forward_store {
    ($(fn $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)*) => {
        $(fn $method(&self, $($arg: $ty),*) -> ThreadStoreFuture<'_, $result> {
            self.inner.$method($($arg),*)
        })*
    };
}

impl ThreadStore for RecoveryFaultStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    forward_store! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> ();
        fn append_items(params: AppendThreadItemsParams) -> ();
        fn append_completion_items_and_flush(params: AppendThreadItemsParams) -> ();
        fn persist_thread(thread_id: ThreadId, context: PersistContext) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn load_sub_agent_completion_context_item(params: LoadSubAgentCompletionContextItemParams) -> Option<codex_protocol::models::ResponseItem>;
        fn load_sub_agent_completion_presentation(params: LoadSubAgentCompletionPresentationParams) -> StoredSubAgentCompletionPresentation;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn update_thread_metadata(params: UpdateThreadMetadataParams) -> Option<StoredThread>;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
    }

    fn accept_mailbox_input_with_authority(
        &self,
        params: AcceptMailboxInputParams,
        authority: MailboxFinalSubscriptionAuthority,
    ) -> ThreadStoreFuture<'_, StoredMailboxInput> {
        Box::pin(async move {
            self.acceptance_attempts
                .fetch_add(/*val*/ 1, Ordering::SeqCst);
            match self.fault {
                AcceptanceFault::Unsupported => Err(ThreadStoreError::Unsupported {
                    operation: "accept_mailbox_input_with_authority",
                }),
                AcceptanceFault::AcknowledgementLost => {
                    self.inner
                        .accept_mailbox_input_with_authority(params, authority)
                        .await?;
                    Err(ThreadStoreError::Internal {
                        message: "acceptance committed but its acknowledgement was lost".into(),
                    })
                }
            }
        })
    }

    fn lookup_mailbox_input<'a>(
        &'a self,
        _receiver_thread_id: ThreadId,
        _submission_key: &'a str,
    ) -> ThreadStoreFuture<'a, Option<StoredMailboxInput>> {
        Box::pin(async move {
            self.lookup_attempts.fetch_add(/*val*/ 1, Ordering::SeqCst);
            self.lookup_started.notify_one();
            Err(ThreadStoreError::Internal {
                message: "acceptance lookup remains unavailable".into(),
            })
        })
    }
}

#[tokio::test]
async fn acceptance_recovery_releases_closed_sender_without_rewriting_or_retiring_intent() {
    for fault in [
        AcceptanceFault::Unsupported,
        AcceptanceFault::AcknowledgementLost,
    ] {
        let home = tempfile::tempdir().expect("test home");
        let mut config = crate::config::ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .cli_overrides(vec![(
                "model".into(),
                toml::Value::String("gpt-5.5".into()),
            )])
            .build()
            .await
            .expect("test config");
        config.cwd = config.codex_home.clone();
        let state = crate::init_state_db(&config)
            .await
            .expect("graph authority");
        let store = Arc::new(RecoveryFaultStore {
            inner: InMemoryThreadStore::default(),
            fault,
            acceptance_attempts: AtomicUsize::new(/*v*/ 0),
            lookup_attempts: AtomicUsize::new(/*v*/ 0),
            lookup_started: Notify::new(),
        });
        let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let manager = crate::thread_manager::ThreadManager::new(
            &config,
            Arc::clone(&auth),
            crate::thread_manager::build_models_manager(&config, auth),
            crate::CodexAppsToolsCache::default(),
            SessionSource::Exec,
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            codex_extension_api::empty_extension_registry(),
            Arc::new(crate::test_support::EmptyUserInstructionsProvider),
            /*analytics_events_client*/ None,
            crate::thread_manager::passthrough_image_store(),
            store.clone(),
            crate::thread_manager::local_agent_graph_store_from_state_db(Some(&state)),
            Uuid::new_v4().to_string(),
            /*attestation_provider*/ None,
            /*external_time_provider*/ None,
        );
        let source = manager
            .start_thread(crate::thread_manager::StartThreadOptions {
                environments: Some(Vec::new()),
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..crate::thread_manager::StartThreadOptions::new(config)
            })
            .await
            .expect("start the exact observer without sampling")
            .thread;
        let control = source
            .session
            .services
            .local_agent_runtime
            .control(source.session.session_id());
        let receiver = ThreadId::new();
        let identity = |thread_id| AgentInputIdentity {
            thread_id,
            nickname: None,
            agent_ref: None,
            task_path: None,
            role: None,
            model: None,
            reasoning_effort: None,
        };
        let params = AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "acknowledgement-loss".into(),
            final_subscription: MailboxFinalSubscriptionRequest::Wake,
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "keep accepted intent".into(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(source.session.thread_id()),
                    recipient: identity(receiver),
                    sender_turn_id: "origin".into(),
                    batch_id: None,
                }),
            },
        };
        // This is the same completion-admission ownership held by the retained acceptance
        // worker. Capture it before scheduling, and keep it until recovery returns.
        let admitted = source
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("acceptance owns a sender drain receipt");
        let worker_source = Arc::clone(&source);
        let worker_params = params.clone();
        let worker = tokio::spawn(async move {
            let _admitted = admitted;
            control
                .accept_mailbox_subscription(&worker_source, worker_params)
                .await
        });
        if matches!(fault, AcceptanceFault::AcknowledgementLost) {
            tokio::time::timeout(
                Duration::from_secs(/*secs*/ 5),
                store.lookup_started.notified(),
            )
            .await
            .expect("ambiguous acceptance reached read-only recovery");
            assert!(!worker.is_finished());
            source
                .session
                .submission_admission
                .close_completion_admission();
        }
        let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), worker)
            .await
            .expect("unsupported or closing acceptance must release its worker")
            .expect("acceptance worker joined");
        assert_eq!(store.acceptance_attempts.load(Ordering::SeqCst), 1);
        match fault {
            AcceptanceFault::Unsupported => {
                assert!(matches!(result, Err(error)
                    if matches!(error.details(), CodexErrorDetails::UnsupportedOperation(_))));
                assert_eq!(store.lookup_attempts.load(Ordering::SeqCst), 0);
                assert!(!source.session.submission_admission.requires_reload());
                assert_eq!(
                    store
                        .inner
                        .lookup_mailbox_input(receiver, &params.submission_key)
                        .await
                        .expect("no acceptance occurred"),
                    None
                );
            }
            AcceptanceFault::AcknowledgementLost => {
                assert!(matches!(result, Err(error)
                    if matches!(error.details(), CodexErrorDetails::Fatal(message)
                        if message.contains("acceptance outcome unknown"))));
                assert!(source.session.submission_admission.requires_reload());
                let accepted = store
                    .inner
                    .lookup_mailbox_input(receiver, &params.submission_key)
                    .await
                    .expect("read original storage directly")
                    .expect("accepted message remains");
                assert_eq!(accepted.payload, params.payload);
                assert_eq!(
                    accepted
                        .final_subscription
                        .as_ref()
                        .map(|token| token.state),
                    Some(MailboxFinalSubscriptionState::Pending)
                );
                assert_eq!(
                    store
                        .inner
                        .lookup_active_mailbox_final_subscription(
                            receiver,
                            source.session.thread_id(),
                        )
                        .await
                        .expect("active intent was not retired"),
                    accepted.final_subscription
                );
            }
        }
        // A leaked accepted-completion guard would keep this shutdown waiting forever.
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), source.shutdown_and_wait())
            .await
            .expect("accepted completion ownership drained")
            .expect("shutdown source");
    }
}
