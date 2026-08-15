//! Public RPC failure cases must share the process-local fault-injection store.

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::write_models_cache;
use codex_app_server::in_process;
use codex_app_server::in_process::InProcessClientHandle;
use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server_protocol::AgentControlAction;
use codex_app_server_protocol::AgentControlOutcome;
use codex_app_server_protocol::AgentControlParams;
use codex_app_server_protocol::AgentControlResponse;
use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentForkMode;
use codex_app_server_protocol::AgentInputOutcome;
use codex_app_server_protocol::AgentResponseHandling;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnEnvironmentParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_features::Feature;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use core_test_support::responses;
use core_test_support::test_codex::test_env;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailurePhase {
    BeforeAdmission,
    AfterAdmission,
}

struct StoreRegistration(String);

impl Drop for StoreRegistration {
    fn drop(&mut self) {
        InMemoryThreadStore::remove_id(&self.0);
    }
}

async fn request<T: DeserializeOwned>(
    client: &InProcessClientHandle,
    request: ClientRequest,
) -> Result<T> {
    let response = timeout(READ_TIMEOUT, client.request(request))
        .await??
        .map_err(|error| anyhow::anyhow!("RPC failed: {error:?}"))?;
    serde_json::from_value(response).context("decode RPC response")
}

#[test_case(FailurePhase::BeforeAdmission, InMemoryThreadStoreFailure::SubAgentCompletionAppend; "before admission append fails")]
#[test_case(FailurePhase::BeforeAdmission, InMemoryThreadStoreFailure::SubAgentCompletionPrefix; "before admission partial prefix")]
#[test_case(FailurePhase::BeforeAdmission, InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush; "before admission flush acknowledgement fails")]
#[test_case(FailurePhase::AfterAdmission, InMemoryThreadStoreFailure::SubAgentCompletionAppend; "after admission append fails")]
#[test_case(FailurePhase::AfterAdmission, InMemoryThreadStoreFailure::SubAgentCompletionPrefix; "after admission partial prefix")]
#[test_case(FailurePhase::AfterAdmission, InMemoryThreadStoreFailure::SubAgentCompletionPresentationFlush; "after admission flush acknowledgement fails")]
#[tokio::test]
async fn queued_prompt_publication_failure_requires_reload(
    phase: FailurePhase,
    failure: InMemoryThreadStoreFailure,
) -> Result<()> {
    const QUEUED_PROMPT: &str = "run the queued fault-injection task";
    const CHILD_RESULT: &str = "the admitted queued task completed";

    let server = responses::start_mock_server().await;
    let child_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, QUEUED_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("queued-child"),
            responses::ev_assistant_message("queued-child-message", CHILD_RESULT),
            responses::ev_completed("queued-child"),
        ]),
    )
    .await;
    let root_wake = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, CHILD_RESULT),
        responses::sse(vec![
            responses::ev_response_created("unexpected-root-wake"),
            responses::ev_assistant_message("unexpected-root-message", "unexpected wake"),
            responses::ev_completed("unexpected-root-wake"),
        ]),
    )
    .await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let _store_registration = StoreRegistration(store_id.clone());
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .with_root_config(&format!(
            r#"experimental_thread_store = {{ type = "in_memory", id = "{store_id}" }}"#
        ))
        .write(codex_home.path())?;
    write_models_cache(codex_home.path()).await?;
    let loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .loader_overrides(loader_overrides.clone())
        .build()
        .await?;
    let state_db = codex_rollout::state_db::try_init(&config).await?;
    let env = test_env().await?;
    let environment_manager = match env.exec_server_url() {
        Some(url) => {
            EnvironmentManager::create_for_tests(
                Some(url.to_string()),
                /*local_runtime_paths*/ None,
            )
            .await
        }
        None => EnvironmentManager::default_for_tests(),
    };
    let mut client = in_process::start(InProcessStartArgs {
        arg0_paths: Arg0DispatchPaths::default(),
        config: Arc::new(config),
        cli_overrides: Vec::new(),
        loader_overrides,
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        thread_config_loader: Arc::new(NoopThreadConfigLoader),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db: Some(state_db),
        environment_manager: Arc::new(environment_manager),
        config_warnings: Vec::new(),
        session_source: SessionSource::Cli,
        enable_codex_api_key_env: false,
        initialize: InitializeParams {
            client_info: ClientInfo {
                name: "codex-app-server-tests".to_string(),
                title: None,
                version: "test".to_string(),
            },
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        },
        channel_capacity: in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    })
    .await?;
    let root: ThreadStartResponse = request(
        &client,
        ClientRequest::ThreadStart {
            request_id: RequestId::Integer(1),
            params: ThreadStartParams {
                environments: Some(vec![TurnEnvironmentParams {
                    environment_id: env.selection().environment_id.clone(),
                    cwd: env.selection().cwd.clone().into(),
                    runtime_workspace_roots: None,
                }]),
                ..Default::default()
            },
        },
    )
    .await?;
    let reserved_handling = match phase {
        FailurePhase::BeforeAdmission => AgentResponseHandling::Wake,
        FailurePhase::AfterAdmission => AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::Wake,
            /*target_messages*/ true,
            /*queue_input*/ false,
        ),
    };
    let spawned: AgentControlResponse = request(
        &client,
        ClientRequest::AgentControl {
            request_id: RequestId::Integer(2),
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some("new".to_string()),
                action: AgentControlAction::Spawn {
                    task: None,
                    role: None,
                    model: None,
                    reasoning_effort: None,
                    input: None,
                    fork_mode: AgentForkMode::None,
                    response_handling: Some(reserved_handling),
                },
            },
        },
    )
    .await?;
    assert_eq!(spawned.audit_warning, None);
    let AgentControlOutcome::Spawned {
        target_thread_id, ..
    } = spawned.outcome
    else {
        anyhow::bail!("user control did not spawn the idle V1 child");
    };
    // This is also evidence that the RPC runtime shares the injected store.
    let barriers_before = store.calls().await.append_completion_items_and_flush;
    assert!(barriers_before > 0, "server did not use the fixture store");
    let (successful_barriers, handling, expected_warning) = match phase {
        FailurePhase::BeforeAdmission => (
            0,
            AgentResponseHandling::Presentation,
            "Queued submission stopped",
        ),
        FailurePhase::AfterAdmission => (
            1,
            AgentResponseHandling::Wake,
            "target input was already admitted; response observation failed",
        ),
    };
    store
        .fail_observation_barrier_after(successful_barriers, failure)
        .await;
    let queued: AgentControlResponse = request(
        &client,
        ClientRequest::AgentControl {
            request_id: RequestId::Integer(3),
            params: AgentControlParams {
                source_thread_id: root.thread.id.clone(),
                authored_selector: Some(target_thread_id.clone()),
                action: AgentControlAction::QueuedPrompt {
                    target: target_thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: QUEUED_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    response_handling: Some(handling),
                },
            },
        },
    )
    .await?;
    assert!(matches!(
        queued.outcome,
        AgentControlOutcome::Prompted {
            target_thread_id: ref prompted_thread_id,
            input_outcome: AgentInputOutcome::Queued,
            post_admission_warning: None,
            ..
        } if prompted_thread_id == &target_thread_id
    ));

    // Keep all relevant events in one wait: the warning and target notifications
    // may arrive in either order, and consuming one must not discard the other.
    timeout(READ_TIMEOUT, async {
        let mut warned = false;
        let mut started = false;
        let mut completed = false;
        loop {
            let event = client.next_event().await.context("app-server stopped")?;
            let notification = match event {
                InProcessServerEvent::ServerNotification(notification) => notification,
                InProcessServerEvent::RequestCompleted { .. } => continue,
                event @ (InProcessServerEvent::ServerRequest(_)
                | InProcessServerEvent::Lagged { .. }) => {
                    anyhow::bail!("unexpected event while waiting for queue outcome: {event:?}");
                }
            };
            match *notification {
                ServerNotification::Warning(warning)
                    if warning.thread_id.as_deref() == Some(root.thread.id.as_str())
                        && warning.message.contains(expected_warning) =>
                {
                    warned = true;
                }
                ServerNotification::TurnStarted(turn) if turn.thread_id == target_thread_id => {
                    assert!(
                        phase == FailurePhase::AfterAdmission,
                        "failed pre-admission publication must not start target work"
                    );
                    assert_eq!(
                        turn.agent_queue
                            .context("queued turn provenance")?
                            .response_handling,
                        None,
                        "queue provenance must not advertise an unacknowledged response policy"
                    );
                    started = true;
                }
                ServerNotification::TurnCompleted(turn) if turn.thread_id == target_thread_id => {
                    assert_eq!(turn.turn.status, TurnStatus::Completed);
                    completed = true;
                }
                _ => {}
            }
            if warned && (phase == FailurePhase::BeforeAdmission || (started && completed)) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;

    let rejected = timeout(
        READ_TIMEOUT,
        client.request(ClientRequest::TurnStart {
            request_id: RequestId::Integer(4),
            params: TurnStartParams {
                thread_id: root.thread.id,
                input: vec![UserInput::Text {
                    text: "quarantined history must reject new input".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        }),
    )
    .await??
    .expect_err("quarantined source must reject more work");
    assert!(
        rejected
            .message
            .contains("thread history must be reloaded before accepting more work"),
        "unexpected admission error: {rejected:?}"
    );
    assert!(
        store.calls().await.append_completion_items_and_flush > barriers_before,
        "the injected canonical barrier was never reached"
    );
    match phase {
        FailurePhase::BeforeAdmission => assert!(
            child_turn.requests().is_empty(),
            "failed observation admission must not submit queued input"
        ),
        FailurePhase::AfterAdmission => {
            child_turn.single_request();
        }
    }
    assert!(
        root_wake.requests().is_empty(),
        "unacknowledged observation must not wake the source"
    );
    client.shutdown().await?;
    Ok(())
}
