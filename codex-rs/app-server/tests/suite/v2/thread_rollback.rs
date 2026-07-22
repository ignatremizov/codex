use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::to_response;
use codex_app_server::in_process;
use codex_app_server::in_process::InProcessClientHandle;
use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::DeprecationNoticeNotification;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::THREAD_ROLLBACK_COMMITTED_ERROR_DATA_FIELD;
use codex_app_server_protocol::THREAD_ROLLBACK_REFRESH_REQUIRED_ERROR_DATA_FIELD;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadRollbackParams;
use codex_app_server_protocol::ThreadRollbackResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_rollout::find_thread_path_by_id_str;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_rollback_rejects_paginated_thread() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(start_resp)?;

    let rollback_id = mcp
        .send_thread_rollback_request(ThreadRollbackParams {
            thread_id: thread.id,
            num_turns: 1,
            expected_start_turn_id: None,
            expected_turn_count: None,
        })
        .await?;
    let rollback_err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(rollback_id)),
    )
    .await??;
    assert_eq!(rollback_err.error.code, -32600);
    assert_eq!(
        rollback_err.error.message,
        "paginated threads do not support thread/rollback"
    );

    Ok(())
}

#[tokio::test]
async fn thread_rollback_does_not_emit_deprecation_notice_to_codex_tui() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let initialized = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_client_info(ClientInfo {
            name: "codex-tui".to_string(),
            title: None,
            version: "0.1.0".to_string(),
        }),
    )
    .await??;
    let JSONRPCMessage::Response(_) = initialized else {
        panic!("expected initialize response, got {initialized:?}");
    };
    mcp.clear_message_buffer();

    let rollback_id = mcp
        .send_thread_rollback_request(ThreadRollbackParams {
            thread_id: "00000000-0000-0000-0000-000000000001".to_string(),
            num_turns: 1,
            expected_start_turn_id: None,
            expected_turn_count: None,
        })
        .await?;
    loop {
        let message = timeout(DEFAULT_READ_TIMEOUT, mcp.read_next_message()).await??;
        match message {
            JSONRPCMessage::Notification(notification) => {
                assert_ne!(notification.method, "deprecationNotice");
            }
            JSONRPCMessage::Error(error) if error.id == RequestId::Integer(rollback_id) => {
                break;
            }
            message => {
                panic!("expected rollback error response, got {message:?}");
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn thread_rollback_errors_distinguish_committed_from_refresh_required() -> Result<()> {
    for (failure, expected_field, unexpected_field) in [
        (
            InMemoryThreadStoreFailure::ThreadRollbackResponseRead,
            THREAD_ROLLBACK_COMMITTED_ERROR_DATA_FIELD,
            THREAD_ROLLBACK_REFRESH_REQUIRED_ERROR_DATA_FIELD,
        ),
        (
            InMemoryThreadStoreFailure::ThreadRollbackVerificationRead,
            THREAD_ROLLBACK_REFRESH_REQUIRED_ERROR_DATA_FIELD,
            THREAD_ROLLBACK_COMMITTED_ERROR_DATA_FIELD,
        ),
        (
            InMemoryThreadStoreFailure::ThreadRollbackFlush,
            THREAD_ROLLBACK_REFRESH_REQUIRED_ERROR_DATA_FIELD,
            THREAD_ROLLBACK_COMMITTED_ERROR_DATA_FIELD,
        ),
    ] {
        let server = create_mock_responses_server_repeating_assistant("Done").await;
        let codex_home = TempDir::new()?;
        let store_id = Uuid::new_v4().to_string();
        create_config_toml_with_in_memory_thread_store(
            codex_home.path(),
            &server.uri(),
            &store_id,
        )?;
        let store = InMemoryThreadStore::for_id(store_id.clone());
        let _store_guard = InMemoryThreadStoreId { store_id };

        let mut client = start_in_process_server(codex_home.path()).await?;
        let start_response = client
            .request(ClientRequest::ThreadStart {
                request_id: RequestId::Integer(1),
                params: ThreadStartParams {
                    model: Some("mock-model".to_string()),
                    ..Default::default()
                },
            })
            .await?
            .map_err(|error| anyhow::anyhow!("thread/start failed: {}", error.message))?;
        let ThreadStartResponse { thread, .. } = serde_json::from_value(start_response)?;

        let turn_response = client
            .request(ClientRequest::TurnStart {
                request_id: RequestId::Integer(2),
                params: TurnStartParams {
                    thread_id: thread.id.clone(),
                    input: vec![V2UserInput::Text {
                        text: "roll this back".to_string(),
                        text_elements: Vec::new(),
                    }],
                    ..Default::default()
                },
            })
            .await?
            .map_err(|error| anyhow::anyhow!("turn/start failed: {}", error.message))?;
        let TurnStartResponse { turn } = serde_json::from_value(turn_response)?;
        timeout(
            DEFAULT_READ_TIMEOUT,
            wait_for_turn_completed(&mut client, thread.id.as_str()),
        )
        .await??;

        store.fail_next_operation(failure).await;
        let rollback_result = client
            .request(ClientRequest::ThreadRollback {
                request_id: RequestId::Integer(3),
                params: ThreadRollbackParams {
                    thread_id: thread.id.clone(),
                    num_turns: 1,
                    expected_start_turn_id: Some(turn.id),
                    expected_turn_count: Some(1),
                },
            })
            .await?;
        let rollback_error = rollback_result.expect_err("thread/rollback should return an error");

        assert_eq!(rollback_error.code, -32603);
        let error_data = rollback_error
            .data
            .as_ref()
            .context("rollback error should include outcome data")?;
        assert_eq!(error_data.get(expected_field), Some(&Value::Bool(true)));
        assert_eq!(error_data.get(unexpected_field), None);

        if failure != InMemoryThreadStoreFailure::ThreadRollbackResponseRead {
            let follow_up = client
                .request(ClientRequest::TurnStart {
                    request_id: RequestId::Integer(4),
                    params: TurnStartParams {
                        thread_id: thread.id.clone(),
                        input: vec![V2UserInput::Text {
                            text: "must not run against indeterminate history".to_string(),
                            text_elements: Vec::new(),
                        }],
                        ..Default::default()
                    },
                })
                .await?;
            let follow_up_error =
                follow_up.expect_err("the indeterminate live thread should be unloaded");
            assert_eq!(follow_up_error.code, -32600);
        }
        client.shutdown().await?;
    }

    Ok(())
}

#[tokio::test]
async fn thread_rollback_drops_last_turns_and_persists_to_rollout() -> Result<()> {
    // Five Codex turns hit the mock model (session start, two prompts, compaction, final prompt).
    let responses = vec![
        create_final_assistant_message_sse_response("Done")?,
        create_final_assistant_message_sse_response("Done")?,
        create_final_assistant_message_sse_response("Done")?,
        create_final_assistant_message_sse_response("Done")?,
        create_final_assistant_message_sse_response("Done")?,
    ];
    let server = create_mock_responses_server_sequence_unchecked(responses).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    // Start a thread.
    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;

    // Two turns.
    let first_text = "First";
    let turn1_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: first_text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _turn1_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn1_id)),
    )
    .await??;
    let _completed1 = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let turn2_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Second".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn2_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn2_id)),
    )
    .await??;
    let TurnStartResponse { turn: turn2 } = to_response(turn2_resp)?;
    let _completed2 = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    mcp.clear_message_buffer();

    // Add a real materialized compaction turn, which has no instruction boundary.
    let compact_id = mcp
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let compact_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(compact_id)),
    )
    .await??;
    let _: ThreadCompactStartResponse = to_response(compact_resp)?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    mcp.clear_message_buffer();

    // Reject a stale thread/read target instead of applying its count to the new suffix.
    let stale_rollback_id = mcp
        .send_thread_rollback_request(ThreadRollbackParams {
            thread_id: thread.id.clone(),
            num_turns: 2,
            expected_start_turn_id: Some(turn2.id.clone()),
            expected_turn_count: Some(2),
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.read_next_message()).await??;
    let _: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(stale_rollback_id)),
    )
    .await??;

    // Roll back the compaction-only turn and the second prompt turn.
    let rollback_id = mcp
        .send_thread_rollback_request(ThreadRollbackParams {
            thread_id: thread.id.clone(),
            num_turns: 2,
            expected_start_turn_id: Some(turn2.id),
            expected_turn_count: Some(3),
        })
        .await?;
    let deprecation_notice = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("deprecationNotice"),
    )
    .await??;
    assert_eq!(deprecation_notice.method, "deprecationNotice");
    let deprecation_notice: DeprecationNoticeNotification = serde_json::from_value(
        deprecation_notice
            .params
            .expect("deprecationNotice params should be present"),
    )?;
    assert_eq!(
        deprecation_notice,
        DeprecationNoticeNotification {
            summary: "thread/rollback is deprecated and will be removed soon".to_string(),
            details: None,
        }
    );
    let rollback_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(rollback_id)),
    )
    .await??;
    let rollback_result = rollback_resp.result.clone();
    let ThreadRollbackResponse {
        thread: rolled_back_thread,
    } = to_response::<ThreadRollbackResponse>(rollback_resp)?;

    // Wire contract: thread title field is `name`, serialized as null when unset.
    let thread_json = rollback_result
        .get("thread")
        .and_then(Value::as_object)
        .expect("thread/rollback result.thread must be an object");
    assert_eq!(rolled_back_thread.name, None);
    assert_eq!(rolled_back_thread.session_id, thread.session_id);
    assert_eq!(
        thread_json.get("name"),
        Some(&Value::Null),
        "thread/rollback must serialize `name: null` when unset"
    );
    assert_eq!(
        thread_json.get("sessionId").and_then(Value::as_str),
        Some(thread.session_id.as_str())
    );

    let rollout_path = find_thread_path_by_id_str(
        codex_home.path(),
        thread.id.as_str(),
        /*state_db_ctx*/ None,
    )
    .await?
    .context("missing rollout after materialized rollback")?;
    let rollout = tokio::fs::read_to_string(rollout_path).await?;
    let rollback_event = rollout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<RolloutLine>(line).ok())
        .find_map(|line| match line.item {
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)) => Some(event),
            _ => None,
        })
        .context("missing durable rollback event")?;
    assert_eq!(rollback_event.num_turns, 1);
    assert_eq!(rollback_event.materialized_turns, Some(2));
    assert!(rollback_event.rollback_start_index.is_some());

    assert_eq!(rolled_back_thread.turns.len(), 1);
    assert_eq!(rolled_back_thread.status, ThreadStatus::Idle);
    assert_eq!(rolled_back_thread.turns[0].items.len(), 2);
    match &rolled_back_thread.turns[0].items[0] {
        ThreadItem::UserMessage { content, .. } => {
            assert_eq!(
                content,
                &vec![V2UserInput::Text {
                    text: first_text.to_string(),
                    text_elements: Vec::new(),
                }]
            );
        }
        other => panic!("expected user message item, got {other:?}"),
    }

    // Resume and confirm the history is pruned.
    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id,
            ..Default::default()
        })
        .await?;
    let resume_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(resume_id)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response::<ThreadResumeResponse>(resume_resp)?;

    assert_eq!(thread.turns.len(), 1);
    assert_eq!(thread.status, ThreadStatus::Idle);
    assert_eq!(thread.turns[0].items.len(), 2);
    match &thread.turns[0].items[0] {
        ThreadItem::UserMessage { content, .. } => {
            assert_eq!(
                content,
                &vec![V2UserInput::Text {
                    text: first_text.to_string(),
                    text_elements: Vec::new(),
                }]
            );
        }
        other => panic!("expected user message item, got {other:?}"),
    }

    let third_turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Third".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(third_turn_id)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = server
        .received_requests()
        .await
        .context("failed to read mock model requests")?;
    let last_model_request = requests
        .iter()
        .rev()
        .find(|request| request.url.path().ends_with("/responses"))
        .context("missing model request after rollback")?;
    let request_body = last_model_request.body_json::<Value>()?;
    let serialized_input = serde_json::to_string(&request_body["input"])?;
    assert!(serialized_input.contains(first_text));
    assert!(serialized_input.contains("Third"));
    assert!(!serialized_input.contains("Second"));

    Ok(())
}

fn create_config_toml(codex_home: &std::path::Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}

fn create_config_toml_with_in_memory_thread_store(
    codex_home: &std::path::Path,
    server_uri: &str,
    store_id: &str,
) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
experimental_thread_store = {{ type = "in_memory", id = "{store_id}" }}

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}

struct InMemoryThreadStoreId {
    store_id: String,
}

impl Drop for InMemoryThreadStoreId {
    fn drop(&mut self) {
        InMemoryThreadStore::remove_id(&self.store_id);
    }
}

async fn start_in_process_server(codex_home: &Path) -> Result<InProcessClientHandle> {
    let loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .fallback_cwd(Some(codex_home.to_path_buf()))
            .loader_overrides(loader_overrides.clone())
            .build()
            .await?,
    );
    Ok(in_process::start(InProcessStartArgs {
        arg0_paths: Arg0DispatchPaths::default(),
        config,
        cli_overrides: Vec::new(),
        loader_overrides,
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        thread_config_loader: Arc::new(NoopThreadConfigLoader),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db: None,
        environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
        config_warnings: Vec::new(),
        session_source: SessionSource::Cli,
        enable_codex_api_key_env: false,
        initialize: InitializeParams {
            client_info: ClientInfo {
                name: "codex-app-server-tests".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            capabilities: None,
        },
        channel_capacity: in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    })
    .await?)
}

async fn wait_for_turn_completed(
    client: &mut InProcessClientHandle,
    thread_id: &str,
) -> Result<()> {
    loop {
        let event = client
            .next_event()
            .await
            .context("in-process app-server stopped before turn/completed")?;
        if let InProcessServerEvent::ServerNotification(ServerNotification::TurnCompleted(
            completed,
        )) = event
            && completed.thread_id == thread_id
        {
            return Ok(());
        }
    }
}
