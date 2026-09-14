use super::*;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::ThreadManager;
use crate::thread_manager::ThreadRegistration;
use codex_history::InitialHistory;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::MailboxFinalSubscriptionRequest;
use codex_thread_store::MailboxPayload;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn retrying_binding_releases_a_closing_observer_without_retiring_durable_intent() {
    let home = tempfile::tempdir().expect("test home");
    let mut config = crate::config::test_config().await;
    config.codex_home = home.path().to_path_buf().try_into().expect("absolute home");
    config.cwd = config.codex_home.clone();
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let store = Arc::new(InMemoryThreadStore::default());
    // No durable graph authority: a real active token must remain retryable while
    // the observer is live, without inventing epochs from canonical history.
    let manager = ThreadManager::new(
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
        /*agent_graph_store*/ None,
        Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let root = manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("start observer without sampling");
    let control = root
        .thread
        .session
        .services
        .local_agent_runtime
        .control(root.thread.session.session_id());
    control
        .runtime
        .register_session_root(root.thread_id, /*current_parent_thread_id*/ None);
    let target = control
        .runtime
        .upgrade()
        .expect("manager")
        .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
            registration: ThreadRegistration::Immediate,
            ownership_override: None,
            config,
            initial_history: InitialHistory::New,
            agent_control: control.clone(),
            session_source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            parent_thread_id: Some(root.thread_id),
            environment_selections: Some(Vec::new()),
            inherited_environments: None,
            inherited_instructions: None,
            inherited_exec_policy: None,
            client_mcp_extensions_override: Some(ClientMcpExtensions::default()),
        })
        .await
        .expect("register the exact native receiver");
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let accepted = store.accept_mailbox_input_with_authority(AcceptMailboxInputParams {
        receiver_thread_id: target.thread_id,
        submission_key: "retry-until-observer-closes".into(),
        payload: MailboxPayload::Agent {
            input: vec![UserInput::Text { text: "retained intent".into(), text_elements: Vec::new() }],
            attribution: Box::new(AgentInputAttribution {
                sender: identity(root.thread_id),
                recipient: identity(target.thread_id),
                sender_turn_id: "sender-turn".into(),
            }),
        },
        final_subscription: MailboxFinalSubscriptionRequest::Wake,
    }, MailboxFinalSubscriptionAuthority {
        receiver_lifecycle_epoch: 0,
        sender_lifecycle_epoch: 0,
    }).await.expect("store immutable acceptance");
    let subscription = accepted.final_subscription.expect("pending final token");
    let mut binding = Box::pin(control.bind_mailbox_final_subscription(
        target.thread.session.presentation_id(), subscription.clone(),
    ));
    assert!(tokio::time::timeout(std::time::Duration::from_millis(/*millis*/ 25), &mut binding)
        .await.is_err(), "missing graph authority must not be treated as a successful binding");
    root.thread.session.submission_admission.close_completion_admission();
    assert!(root.thread.session.submission_admission.check_ready().is_ok());
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 3), binding)
        .await.expect("retry loop must release the closed observer");
    assert_eq!(store.lookup_active_mailbox_final_subscription(target.thread_id, root.thread_id)
        .await.expect("durable token remains available for later exact-instance recovery"),
        Some(subscription));
    target.thread.shutdown_and_wait().await.expect("shutdown receiver");
    root.thread.shutdown_and_wait().await.expect("shutdown observer");
}
