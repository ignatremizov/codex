use super::*;
use crate::app_event::TranscriptExportDestination;
use crate::chatwidget::UserMessage;
use crate::chatwidget::agent_command::AgentSelector;
use crate::chatwidget::agent_command::AgentSelectorKind;
use app_test_support::create_fake_paginated_rollout;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ReviewTarget as AppServerReviewTarget;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItemsListParams;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::AgentPath;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::EnteredReviewModeItem;
use codex_protocol::items::ExitedReviewModeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::protocol::SessionSource as RolloutSessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::sub_agent_completion_item;
use codex_protocol::user_input::UserInput as CoreUserInput;
use codex_state::SqliteConfig;
use core_test_support::responses;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

pub(super) type RecordedRequests = Arc<Mutex<Vec<JSONRPCRequest>>>;
pub(super) type RecordingAppServer = (AppServerSession, RecordedRequests, JoinHandle<Result<()>>);

fn test_transcript_cells(
    thread_id: Option<ThreadId>,
    cwd: &codex_utils_absolute_path::AbsolutePathBuf,
    items: impl IntoIterator<Item = codex_app_server_protocol::ThreadItem>,
    visibility: crate::thread_transcript::RawReasoningVisibility,
    config: Option<&crate::legacy_core::config::Config>,
) -> crate::thread_transcript::TranscriptCells {
    let items = items.into_iter().collect::<Vec<_>>();
    let metadata = crate::thread_transcript::collab_agent_metadata_from_items(&items);
    crate::thread_transcript::thread_items_to_transcript_cells_with_metadata(
        thread_id, cwd, items, visibility, config, &metadata,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryCapabilities {
    Current,
    LegacyOnly,
    LegacyOnlyUnsupportedVariant,
    LegacyDynamicToolsAndHistory,
    ForkHydrationFails,
    ThreadListFails,
}

/// Returns and resets `(thread/loaded/list, thread/read)` request counts.
fn take_backfill_counts(requests: &RecordedRequests) -> (usize, usize) {
    let requests = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
    (
        requests
            .iter()
            .filter(|request| request.method == "thread/loaded/list")
            .count(),
        requests
            .iter()
            .filter(|request| request.method == "thread/read")
            .count(),
    )
}

/// Starts an embedded app server behind a loopback WebSocket proxy that records JSON-RPC methods.
pub(super) async fn start_recording_app_server(
    config: &Config,
    blocked_thread_list: Option<(ThreadId, oneshot::Sender<()>, oneshot::Receiver<()>)>,
    failed_thread_name: Option<&'static str>,
) -> Result<RecordingAppServer> {
    start_recording_app_server_with_history(
        config,
        HistoryCapabilities::Current,
        blocked_thread_list,
        failed_thread_name,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await
}

pub(super) async fn start_recording_remote_app_server(
    config: &Config,
) -> Result<RecordingAppServer> {
    start_recording_app_server_with_history(
        config,
        HistoryCapabilities::Current,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Remote,
    )
    .await
}

/// Proxies a real app server while optionally rejecting modern pagination like an older server.
async fn start_recording_app_server_with_history(
    config: &Config,
    history_capabilities: HistoryCapabilities,
    mut blocked_thread_list: Option<(ThreadId, oneshot::Sender<()>, oneshot::Receiver<()>)>,
    failed_thread_name: Option<&'static str>,
    thread_params_mode: crate::app_server_session::ThreadParamsMode,
) -> Result<RecordingAppServer> {
    let state_db =
        crate::init_state_db_for_app_server_target(config, &crate::AppServerTarget::Embedded)
            .await?;
    let embedded = crate::start_embedded_app_server(
        codex_arg0::Arg0DispatchPaths::default(),
        config.clone(),
        Vec::new(),
        codex_config::LoaderOverrides::default(),
        /*strict_config*/ false,
        codex_config::CloudConfigBundleLoader::default(),
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        state_db,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    )
    .await?;
    let codex_home = config.codex_home.display().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let request_sink = Arc::clone(&requests);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let proxy = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
        let mut inventories = usize::from(failed_thread_name == Some("background"));
        let mut reject_detach = false;
        let mut reject_thread_list = history_capabilities == HistoryCapabilities::ThreadListFails;
        let mut queued_prompt_attempts = HashMap::<String, usize>::new();
        while let Some(frame) = websocket.next().await {
            let Message::Text(text) = frame? else {
                continue;
            };
            let message = serde_json::from_str::<JSONRPCMessage>(&text)?;
            match message {
                JSONRPCMessage::Request(request) if request.method == "initialize" => {
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                                id: request.id,
                                result: serde_json::json!({
                                    "userAgent": "codex-tui-test",
                                    "codexHome": codex_home,
                                }),
                            }))?
                            .into(),
                        ))
                        .await?;
                }
                JSONRPCMessage::Request(request) => {
                    request_sink
                        .lock()
                        .expect("request recorder lock")
                        .push(request.clone());
                    let request_id = request.id.clone();
                    let params = request.params.as_ref();
                    let requires_pagination = match request.method.as_str() {
                        "thread/start" => params
                            .and_then(|params| params.get("historyMode"))
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|mode| mode == "paginated"),
                        "thread/resume" | "thread/fork" => params
                            .and_then(|params| params.get("excludeTurns"))
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                        "thread/turns/list" | "thread/items/list" => true,
                        _ => false,
                    };
                    let reject_fork_hydration = history_capabilities
                        == HistoryCapabilities::ForkHydrationFails
                        && request.method == "thread/items/list"
                        && request_sink
                            .lock()
                            .expect("request recorder lock")
                            .iter()
                            .any(|recorded| recorded.method == "thread/fork");
                    let reject_dynamic_tools = history_capabilities
                        == HistoryCapabilities::LegacyDynamicToolsAndHistory
                        && request.method == "thread/start"
                        && params
                            .and_then(|params| params.get("dynamicTools"))
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|tools| {
                                tools.iter().any(|tool| tool["type"] == "namespace")
                            });
                    let response = if request.method == "thread/list"
                        && std::mem::take(&mut reject_thread_list)
                    {
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32603,
                                data: None,
                                message: "thread listing unavailable".to_string(),
                            },
                        })
                    } else if history_capabilities == HistoryCapabilities::LegacyOnly
                        && request.method == "thread/list"
                        && params.is_some_and(|params| params["sortKey"] == "recency_at")
                    {
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32602,
                                data: None,
                                message: "unknown variant `recency_at`".to_string(),
                            },
                        })
                    } else if reject_dynamic_tools {
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32602,
                                data: None,
                                message: "missing field `inputSchema`".to_string(),
                            },
                        })
                    } else if matches!(
                        history_capabilities,
                        HistoryCapabilities::LegacyOnly
                            | HistoryCapabilities::LegacyOnlyUnsupportedVariant
                            | HistoryCapabilities::LegacyDynamicToolsAndHistory
                    ) && requires_pagination
                    {
                        let (code, message) = if history_capabilities
                            == HistoryCapabilities::LegacyOnlyUnsupportedVariant
                            && request.method == "thread/start"
                        {
                            (-32602, "unknown variant \"paginated\", expected \"legacy\"")
                        } else {
                            (-32601, "method not found")
                        };
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code,
                                data: None,
                                message: message.to_string(),
                            },
                        })
                    } else if reject_fork_hydration {
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32603,
                                data: None,
                                message: "fork history hydration failed".to_string(),
                            },
                        })
                    } else {
                        let background = request.method == "thread/backgroundTerminals/list" && {
                            inventories += usize::from(inventories > 0);
                            matches!(inventories, 2 | 4)
                        };
                        let detach = request.method == "thread/unsubscribe";
                        let request = serde_json::from_value::<ClientRequest>(
                            serde_json::to_value(request)?,
                        )?;
                        if let ClientRequest::ThreadList { params, .. } = &request
                            && let Some((root, started, release)) = blocked_thread_list.take()
                        {
                            assert_eq!(params.ancestor_thread_id, Some(root.to_string()));
                            assert_eq!(params.sort_direction, Some(SortDirection::Desc));
                            let _ = started.send(());
                            let _ = release.await;
                        }
                        let force_failure = matches!(
                            &request,
                            ClientRequest::ThreadSetName { params, .. }
                                if failed_thread_name == Some(params.name.as_str())
                        ) || matches!(
                            &request,
                            ClientRequest::ThreadFork { params, .. }
                                if params.cwd.as_deref().is_some_and(|cwd| cwd.ends_with("failure"))
                                    && { reject_detach = true; true }
                        ) || (detach && std::mem::take(&mut reject_detach));
                        let agent_control_success =
                            |outcome: serde_json::Value, audit_warning: Option<&str>| {
                                Ok::<_, JSONRPCErrorError>(serde_json::json!({
                                    "outcome": outcome,
                                    "auditWarning": audit_warning,
                                }))
                            };
                        let agent_control_response = match &request {
                            ClientRequest::AgentControl { params, .. } => match &params.action {
                                codex_app_server_protocol::AgentControlAction::Spawn {
                                    role,
                                    ..
                                } => {
                                    let target_thread_id = match embedded
                                        .request(ClientRequest::ThreadStart {
                                            request_id: RequestId::Integer(999999),
                                            params: ThreadStartParams::default(),
                                        })
                                        .await
                                    {
                                        Ok(Ok(value)) => {
                                            serde_json::from_value::<ThreadStartResponse>(value)
                                                .map(|res| res.thread.id)
                                                .unwrap_or_else(|_| ThreadId::new().to_string())
                                        }
                                        _ => ThreadId::new().to_string(),
                                    };
                                    Some(agent_control_success(
                                        serde_json::json!({
                                            "type": "spawned",
                                            "targetThreadId": target_thread_id,
                                            "ref": "2",
                                            "nickname": role,
                                        }),
                                        None,
                                    ))
                                }
                                codex_app_server_protocol::AgentControlAction::Prompt {
                                    target,
                                    input,
                                    ..
                                }
                                | codex_app_server_protocol::AgentControlAction::QueuedPrompt {
                                    target,
                                    input,
                                    ..
                                } => {
                                    let queued_prompt = matches!(
                                        &params.action,
                                        codex_app_server_protocol::AgentControlAction::QueuedPrompt {
                                            ..
                                        }
                                    );
                                    let forced_error = input.iter().any(|item| {
                                        matches!(
                                            item,
                                            AppServerUserInput::Text { text, .. }
                                                if text == "must not be reported as sent"
                                        )
                                    });
                                    let foreign_target = input.iter().any(|item| {
                                        matches!(
                                            item,
                                            AppServerUserInput::Text { text, .. }
                                                if text.contains("unrelated thread")
                                        )
                                    });
                                    Some(if forced_error {
                                        Err(JSONRPCErrorError {
                                            code: -32602,
                                            message: "cannot steer a review turn".to_string(),
                                            data: None,
                                        })
                                    } else if foreign_target {
                                        Err(JSONRPCErrorError {
                                            code: -32602,
                                            message: format!(
                                                "agent {target} is not controlled by this root; \
                                                 use resume_agent to adopt it"
                                            ),
                                            data: None,
                                        })
                                    } else {
                                        agent_control_success(
                                            serde_json::json!({
                                                "type": "prompted",
                                                "targetThreadId": target,
                                                "submissionId": "agent-control-submission",
                                                "queued": queued_prompt,
                                            }),
                                            None,
                                        )
                                    })
                                }
                                codex_app_server_protocol::AgentControlAction::ReservedPrompt {
                                    target,
                                    ..
                                } => Some(agent_control_success(
                                    serde_json::json!({
                                        "type": "reservedPrompted",
                                        "targetThreadId": target,
                                        "submissionId": "agent-control-first-submission",
                                        "turnId": "agent-control-first-turn",
                                    }),
                                    None,
                                )),
                                codex_app_server_protocol::AgentControlAction::Resume {
                                    target,
                                    ..
                                } => {
                                    let degraded = params.authored_selector.as_deref()
                                        == Some("degraded-resume");
                                    Some(agent_control_success(
                                        serde_json::json!({
                                            "type": "resumed",
                                            "targetThreadId": target,
                                            "ref": "2",
                                            "nickname": degraded.then_some("Hopper"),
                                            "observationBinding": if degraded {
                                                serde_json::Value::Null
                                            } else {
                                                serde_json::Value::String("nextTurn".to_string())
                                            },
                                            "postCommitWarning": degraded.then_some(
                                                "agent is now owned by this root; retry resume"
                                            ),
                                        }),
                                        None,
                                    ))
                                }
                                codex_app_server_protocol::AgentControlAction::Interrupt {
                                    target,
                                    input,
                                    ..
                                } => Some(agent_control_success(
                                    serde_json::json!({
                                        "type": "interrupted",
                                        "targetThreadId": target,
                                        "submissionId": input
                                            .as_ref()
                                            .map(|_| "agent-interrupt-follow-up"),
                                    }),
                                    None,
                                )),
                                codex_app_server_protocol::AgentControlAction::Close {
                                    target,
                                    ..
                                } => Some(agent_control_success(
                                    serde_json::json!({
                                        "type": "closed",
                                        "targetThreadId": target,
                                    }),
                                    None,
                                )),
                                codex_app_server_protocol::AgentControlAction::Observe {
                                    target,
                                    response_handling,
                                } => Some(agent_control_success(
                                    serde_json::json!({
                                        "type": "observed",
                                        "targetThreadId": target,
                                        "previousResponseHandling": "wake",
                                        "responseHandling": response_handling,
                                        "binding": if params.authored_selector.as_deref()
                                            == Some("undelivered-observation")
                                        {
                                            "undeliveredCompletion"
                                        } else {
                                            "activeTurn"
                                        },
                                    }),
                                    None,
                                )),
                            },
                            _ => None,
                        };
                        let agent_queue_delete_response = match &request {
                            ClientRequest::AgentQueueDelete { params, .. } => {
                                Some(Ok::<_, JSONRPCErrorError>(serde_json::json!({
                                    "id": params.id,
                                })))
                            }
                            _ => None,
                        };
                        if let Some(synthetic_response) =
                            agent_control_response.or(agent_queue_delete_response)
                        {
                            match synthetic_response {
                                Ok(result) => JSONRPCMessage::Response(JSONRPCResponse {
                                    id: request_id,
                                    result,
                                }),
                                Err(error) => JSONRPCMessage::Error(JSONRPCError {
                                    id: request_id,
                                    error,
                                }),
                            }
                        } else if force_failure {
                            JSONRPCMessage::Error(JSONRPCError {
                                id: request_id,
                                error: JSONRPCErrorError {
                                    code: -32603,
                                    message: "forced thread/name/set failure".to_string(),
                                    data: None,
                                },
                            })
                        } else {
                            let mut result = embedded.request(request).await?;
                            if background {
                                let terminal = r#"{"data":[{"itemId":"x","processId":"x","command":"x","cwd":"/"}],"nextCursor":null}"#;
                                result = Ok(serde_json::from_str(terminal)?);
                            }
                            match result {
                                Ok(result) => JSONRPCMessage::Response(JSONRPCResponse {
                                    id: request_id,
                                    result,
                                }),
                                Err(error) => JSONRPCMessage::Error(JSONRPCError {
                                    id: request_id,
                                    error,
                                }),
                            }
                        }
                    };
                    websocket
                        .send(Message::Text(serde_json::to_string(&response)?.into()))
                        .await?;
                }
                JSONRPCMessage::Notification(notification)
                    if notification.method == "initialized" => {}
                JSONRPCMessage::Notification(notification) => {
                    embedded
                        .notify(serde_json::from_value::<ClientNotification>(
                            serde_json::to_value(notification)?,
                        )?)
                        .await?;
                }
                JSONRPCMessage::Response(response) => {
                    request_sink
                        .lock()
                        .expect("request recorder lock")
                        .push(JSONRPCRequest {
                            id: response.id,
                            method: "server/request/response".to_string(),
                            params: Some(response.result),
                            trace: None,
                        });
                }
                JSONRPCMessage::Error(_) => {}
            }
        }
        embedded.shutdown().await?;
        Ok(())
    });
    let app_server = crate::connect_remote_app_server(crate::RemoteAppServerEndpoint::WebSocket {
        websocket_url,
        auth_token: None,
    })
    .await?;

    Ok((
        AppServerSession::new(app_server, thread_params_mode).with_startup_config(config),
        requests,
        proxy,
    ))
}

async fn mount_delayed_agent_prompt_response(server: &MockServer, response_id: &str) {
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse_completed(response_id))
                .set_delay(Duration::from_secs(/*secs*/ 30)),
        )
        .mount(server)
        .await;
}

fn configure_agent_prompt_model_server(app: &mut App, server: &MockServer) {
    app.config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    app.config.model_provider.env_key = None;
    app.config.model_provider.experimental_bearer_token = Some("test-token".to_string().into());
}

fn display_test_thread(app: &mut App, thread_id: ThreadId) {
    app.active_thread_id = Some(thread_id);
    app.chat_widget
        .handle_thread_session(test_thread_session(thread_id, app.config.cwd.to_path_buf()));
}

#[tokio::test]
async fn promptless_spawn_routes_first_child_input_through_reserved_control() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source_thread_id = ThreadId::new();
    display_test_thread(&mut app, source_thread_id);

    let target_thread_id = app
        .spawn_agent_from_command(
            &mut app_server,
            crate::app::SpawnAgentCommandArgs {
                source_thread_id,
                role: None,
                authored_selector: Some("new".to_string()),
                prompt: None,
                fork_mode: codex_app_server_protocol::AgentForkMode::None,
                response_handling: Some(
                    codex_app_server_protocol::AgentResponseHandling::Presentation,
                ),
            },
        )
        .await
        .expect("prompt-less spawn should return a child thread");
    assert_eq!(
        app.agent_navigation
            .reserved_prompt_source(target_thread_id),
        Some(source_thread_id)
    );
    requests.lock().expect("request recorder lock").clear();

    let turn_id = app
        .submit_reserved_agent_prompt(
            &mut app_server,
            source_thread_id,
            target_thread_id,
            vec![AppServerUserInput::Text {
                text: "first child prompt".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await?;
    assert_eq!(turn_id, "agent-control-first-turn");

    let recorded = requests.lock().expect("request recorder lock").clone();
    let request = recorded
        .iter()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("reserved prompt agent/control params");
    assert_eq!(request["sourceThreadId"], source_thread_id.to_string());
    assert_eq!(request["authoredSelector"], serde_json::Value::Null);
    assert_eq!(request["action"]["type"], "reservedPrompt");
    assert_eq!(request["action"]["target"], target_thread_id.to_string());
    assert_eq!(
        request["action"]["input"],
        serde_json::to_value(vec![AppServerUserInput::Text {
            text: "first child prompt".to_string(),
            text_elements: Vec::new(),
        }])?
    );
    assert_eq!(
        app.agent_navigation
            .reserved_prompt_source(target_thread_id),
        None
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn promptless_resume_routes_next_child_input_through_reserved_control() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source_thread_id = ThreadId::new();
    display_test_thread(&mut app, source_thread_id);

    let target_thread_id = app
        .spawn_agent_from_command(
            &mut app_server,
            crate::app::SpawnAgentCommandArgs {
                source_thread_id,
                role: None,
                authored_selector: Some("new".to_string()),
                prompt: None,
                fork_mode: codex_app_server_protocol::AgentForkMode::None,
                response_handling: Some(
                    codex_app_server_protocol::AgentResponseHandling::Presentation,
                ),
            },
        )
        .await
        .expect("prompt-less spawn should return a child thread");
    app.agent_navigation
        .clear_reserved_prompt_response(target_thread_id);
    requests.lock().expect("request recorder lock").clear();

    app.resume_agent_from_selector(
        &mut app_server,
        source_thread_id,
        AgentSelector {
            kind: AgentSelectorKind::Id(target_thread_id),
            authored: target_thread_id.to_string(),
        },
        Some(codex_app_server_protocol::AgentResponseHandling::Wake),
        /*prompt*/ None,
    )
    .await;

    assert_eq!(
        app.agent_navigation
            .reserved_prompt_source(target_thread_id),
        Some(source_thread_id)
    );
    let recorded = requests.lock().expect("request recorder lock").clone();
    let request = recorded
        .iter()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("resume agent/control params");
    assert_eq!(request["sourceThreadId"], source_thread_id.to_string());
    assert_eq!(request["action"]["type"], "resume");
    assert_eq!(
        request["action"]["responseHandling"],
        serde_json::json!({
            "commentary": false,
            "finalResponse": "wake",
            "targetMessages": false,
            "queueInput": false,
        })
    );
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt_to_selector(
        &mut app_server,
        source_thread_id,
        AgentSelector {
            kind: AgentSelectorKind::Id(target_thread_id),
            authored: target_thread_id.to_string(),
        },
        UserMessage {
            text: "continue under the reserved wake".to_string(),
            local_images: Vec::new(),
            remote_image_urls: Vec::new(),
            text_elements: Vec::new(),
            mention_bindings: Vec::new(),
        },
        /*response_handling*/ None,
    )
    .await;

    let recorded = requests.lock().expect("request recorder lock").clone();
    let prompt_request = recorded
        .iter()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("reserved prompt agent/control params");
    assert_eq!(prompt_request["action"]["type"], "reservedPrompt");
    assert_eq!(
        prompt_request["action"]["target"],
        target_thread_id.to_string()
    );
    assert_eq!(
        app.agent_navigation
            .reserved_prompt_source(target_thread_id),
        None
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn observing_undelivered_completion_preserves_next_turn_policy() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, _requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    display_test_thread(&mut app, source_thread_id);
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Hopper".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.agent_navigation.note_response_observation(
        source_thread_id,
        target_thread_id,
        AgentResponseObservationBinding::NextTurn,
        Some(codex_app_server_protocol::AgentResponseHandling::Wake),
    );
    app.agent_navigation.note_response_observation(
        source_thread_id,
        target_thread_id,
        AgentResponseObservationBinding::Bound,
        Some(codex_app_server_protocol::AgentResponseHandling::Presentation),
    );

    app.observe_agent_from_selector(
        &mut app_server,
        source_thread_id,
        AgentSelector {
            kind: AgentSelectorKind::Id(target_thread_id),
            authored: "undelivered-observation".to_string(),
        },
        codex_app_server_protocol::AgentObservationMode::Passive,
    )
    .await;

    assert_eq!(
        app.agent_navigation
            .response_observation(source_thread_id, target_thread_id),
        Some(
            crate::app::agent_observation_display::AgentResponseObservationDisplay {
                binding: AgentResponseObservationBinding::NextTurn,
                commentary: false,
                target_messages: false,
                queue_delivery: false,
                final_response:
                    crate::app::agent_observation_display::AgentFinalResponseDisplay::Wake,
            }
        )
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn degraded_adoption_clears_optimistic_observation_and_renders_recovery() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, _requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source_thread_id = ThreadId::new();
    let target_thread_id = ThreadId::new();
    display_test_thread(&mut app, source_thread_id);
    while app_event_rx.try_recv().is_ok() {}
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Hopper".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ true,
    );

    app.resume_agent_from_selector(
        &mut app_server,
        source_thread_id,
        AgentSelector {
            kind: AgentSelectorKind::Id(target_thread_id),
            authored: "degraded-resume".to_string(),
        },
        Some(codex_app_server_protocol::AgentResponseHandling::Wake),
        /*prompt*/ None,
    )
    .await;

    assert_eq!(
        app.agent_navigation
            .response_observation(source_thread_id, target_thread_id),
        None
    );
    assert_eq!(
        app.agent_navigation
            .reserved_prompt_source(target_thread_id),
        None
    );
    let rendered = std::iter::from_fn(|| app_event_rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 120)))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    ■ Agent ownership changed, but resume setup degraded: agent is now owned by this root; retry resume
    ");

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn direct_agent_prompt_uses_source_relative_control_with_structured_payload() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let model_server = MockServer::start().await;
    mount_delayed_agent_prompt_response(&model_server, "idle-target-response").await;
    configure_agent_prompt_model_server(&mut app, &model_server);
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Robie".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    requests.lock().expect("request recorder lock").clear();

    let local_image_path = app.config.cwd.join("agent-prompt.png");
    let skill_path = app.config.cwd.join("review-skill").join("SKILL.md");
    let prompt = "Use $review, $docs, and @drive";
    let user_message = crate::chatwidget::UserMessage {
        text: prompt.to_string(),
        local_images: vec![crate::bottom_pane::LocalImageAttachment {
            placeholder: "[Image #1]".to_string(),
            path: local_image_path.clone().to_path_buf(),
        }],
        remote_image_urls: vec!["data:image/png;base64,aGVsbG8=".to_string()],
        text_elements: vec![TextElement::new(
            codex_protocol::user_input::ByteRange { start: 13, end: 18 },
            Some("$docs".to_string()),
        )],
        mention_bindings: vec![
            crate::bottom_pane::MentionBinding {
                sigil: '$',
                mention: "review".to_string(),
                path: skill_path.to_string_lossy().into_owned(),
            },
            crate::bottom_pane::MentionBinding {
                sigil: '$',
                mention: "docs".to_string(),
                path: "plugin://docs@personal".to_string(),
            },
            crate::bottom_pane::MentionBinding {
                sigil: '@',
                mention: "drive".to_string(),
                path: "app://drive".to_string(),
            },
        ],
    };
    app.submit_agent_prompt(&mut app_server, target_thread_id, user_message)
        .await;

    assert_eq!(
        app.current_displayed_thread_id(),
        Some(displayed_thread_id),
        "direct prompting must not change the displayed thread"
    );
    let recorded = requests.lock().expect("request recorder lock").clone();
    assert_eq!(
        recorded
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    let agent_control = recorded
        .iter()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("agent/control params");
    assert_eq!(
        agent_control["sourceThreadId"],
        displayed_thread_id.to_string()
    );
    assert_eq!(
        agent_control["authoredSelector"],
        target_thread_id.to_string()
    );
    assert_eq!(agent_control["action"]["type"], "prompt");
    assert_eq!(
        agent_control["action"]["target"],
        target_thread_id.to_string()
    );
    assert_eq!(
        agent_control["action"]["input"],
        serde_json::to_value(
            vec![
                CoreUserInput::Image {
                    image_url: "data:image/png;base64,aGVsbG8=".to_string(),
                    detail: None,
                },
                CoreUserInput::LocalImage {
                    path: local_image_path.to_path_buf(),
                    detail: None,
                },
                CoreUserInput::Text {
                    text: prompt.to_string(),
                    text_elements: vec![codex_protocol::user_input::TextElement::new(
                        codex_protocol::user_input::ByteRange { start: 13, end: 18 },
                        Some("$docs".to_string()),
                    )],
                },
            ]
            .into_iter()
            .map(codex_app_server_protocol::UserInput::from)
            .collect::<Vec<_>>(),
        )?
    );
    assert_eq!(
        agent_control["action"]["responseHandling"],
        serde_json::Value::Null
    );

    app.submit_agent_prompt(
        &mut app_server,
        target_thread_id,
        crate::chatwidget::UserMessage::from("follow up before turn/started"),
    )
    .await;

    let recorded_after_follow_up = requests.lock().expect("request recorder lock").clone();
    assert_eq!(
        recorded_after_follow_up
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec![
            "thread/read",
            "agent/control",
            "thread/read",
            "thread/read",
            "agent/control",
            "thread/read"
        ]
    );
    let follow_up = recorded_after_follow_up
        .iter()
        .rev()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("follow-up agent/control params");
    assert_eq!(
        follow_up["action"]["input"],
        serde_json::to_value(vec![AppServerUserInput::Text {
            text: "follow up before turn/started".to_string(),
            text_elements: Vec::new(),
        }])?
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn direct_agent_prompt_uses_control_api_when_target_turn_started_elsewhere() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let model_server = MockServer::start().await;
    mount_delayed_agent_prompt_response(&model_server, "intervening-turn-response").await;
    configure_agent_prompt_model_server(&mut app, &model_server);
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    let _intervening_turn = app_server
        .turn_start_with_thread_defaults(
            target_thread_id,
            vec![AppServerUserInput::Text {
                text: "intervening writer".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await?;
    time::timeout(Duration::from_secs(/*secs*/ 2), async {
        loop {
            let thread = app_server
                .thread_read(target_thread_id, /*include_turns*/ false)
                .await
                .expect("read intervening turn");
            if matches!(thread.status, ThreadStatus::Active { .. }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Robie".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt(
        &mut app_server,
        target_thread_id,
        crate::chatwidget::UserMessage::from("admit this into the active turn"),
    )
    .await;

    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    assert_eq!(
        app.current_displayed_thread_id(),
        Some(displayed_thread_id),
        "prompt admission must not change focus"
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn direct_agent_prompt_steers_active_target_without_changing_focus() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let model_server = MockServer::start().await;
    mount_delayed_agent_prompt_response(&model_server, "active-target-response").await;
    configure_agent_prompt_model_server(&mut app, &model_server);
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    let started_turn = app_server
        .turn_start_with_thread_defaults(
            target_thread_id,
            vec![AppServerUserInput::Text {
                text: "keep working".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await?;
    time::timeout(Duration::from_secs(/*secs*/ 2), async {
        loop {
            let thread = app_server
                .thread_read(target_thread_id, /*include_turns*/ false)
                .await
                .expect("read active target");
            if matches!(thread.status, ThreadStatus::Active { .. }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Herschel".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.ensure_thread_channel(target_thread_id)
        .store
        .lock()
        .await
        .set_active_turn_id(started_turn.turn.id.clone());
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt(
        &mut app_server,
        target_thread_id,
        crate::chatwidget::UserMessage::from("inspect the latest change"),
    )
    .await;

    assert_eq!(
        app.current_displayed_thread_id(),
        Some(displayed_thread_id),
        "steering another thread must not change focus"
    );
    let recorded = requests.lock().expect("request recorder lock").clone();
    assert_eq!(
        recorded
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    let agent_control = recorded
        .iter()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("agent/control params");
    assert_eq!(
        agent_control["action"]["input"],
        serde_json::to_value(vec![AppServerUserInput::Text {
            text: "inspect the latest change".to_string(),
            text_elements: Vec::new(),
        }])?
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn direct_agent_prompt_reports_non_steerable_admission_failure() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let model_server = MockServer::start().await;
    mount_delayed_agent_prompt_response(&model_server, "review-admission-response").await;
    configure_agent_prompt_model_server(&mut app, &model_server);
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Robie".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    let review = app_server
        .review_start(
            target_thread_id,
            AppServerReviewTarget::Custom {
                instructions: "hold review open".to_string(),
            },
        )
        .await?;
    assert_eq!(review.turn.status, TurnStatus::InProgress);
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt(
        &mut app_server,
        target_thread_id,
        crate::chatwidget::UserMessage::from("must not be reported as sent"),
    )
    .await;

    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    let rendered = std::iter::from_fn(|| app_event_rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 120)))
            }
            _ => None,
        })
        .collect::<String>();
    assert!(
        rendered.contains("cannot steer a review turn"),
        "expected admission failure guidance, got: {rendered}"
    );
    assert!(
        !rendered.contains("Sent user prompt"),
        "rejected prompt must not be reported as sent: {rendered}"
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn direct_agent_prompt_routes_closed_target_through_resume_capable_control() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            app.config.codex_home.as_path(),
            "2026-01-01T00-00-00",
            "2026-01-01T00:00:00Z",
            "agent message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create fake rollout"),
    )?;
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Robie".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.ensure_thread_channel(target_thread_id);
    app_server.thread_archive(target_thread_id).await?;
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt(
        &mut app_server,
        target_thread_id,
        crate::chatwidget::UserMessage::from("this must not resume the target"),
    )
    .await;

    assert_eq!(
        app.current_displayed_thread_id(),
        Some(displayed_thread_id),
        "prompting another thread must not change focus"
    );
    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    assert!(
        app.agent_navigation
            .get(&target_thread_id)
            .expect("closed target remains navigable")
            .is_closed
    );
    assert_eq!(
        app.thread_event_channels
            .get(&target_thread_id)
            .map(ThreadEventChannel::attachment),
        Some(ThreadEventAttachment::ReplayOnly)
    );
    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn agent_prompts_reject_foreign_uuid_with_adoption_guidance() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    display_test_thread(&mut app, displayed_thread_id);
    let unrelated_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt_to_selector(
        &mut app_server,
        displayed_thread_id,
        crate::chatwidget::agent_command::AgentSelector {
            kind: crate::chatwidget::agent_command::AgentSelectorKind::Id(unrelated_thread_id),
            authored: unrelated_thread_id.to_string(),
        },
        crate::chatwidget::UserMessage::from("this must not reach the unrelated thread"),
        /*response_handling*/ None,
    )
    .await;

    assert_eq!(
        app.current_displayed_thread_id(),
        Some(displayed_thread_id),
        "rejecting an unknown thread must not change focus"
    );
    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"],
        "Core must authoritatively reject the foreign prompt"
    );
    assert!(
        app.agent_navigation.get(&unrelated_thread_id).is_some(),
        "an explicitly addressed foreign rollout remains available for read-only inspection"
    );
    let rendered = std::iter::from_fn(|| app_event_rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 120)))
            }
            _ => None,
        })
        .collect::<String>();
    assert!(
        rendered.contains(&format!(
            "agent {unrelated_thread_id} is not controlled by this root"
        )) && rendered.contains(&format!(
            "run `/agent resume {unrelated_thread_id}` to adopt it"
        )),
        "expected explicit adoption guidance, got: {rendered}"
    );

    requests.lock().expect("request recorder lock").clear();
    app.queue_agent_prompt_to_selector(
        &mut app_server,
        displayed_thread_id,
        crate::chatwidget::agent_command::AgentSelector {
            kind: crate::chatwidget::agent_command::AgentSelectorKind::Id(unrelated_thread_id),
            authored: unrelated_thread_id.to_string(),
        },
        crate::chatwidget::UserMessage::from("this must not be queued for the unrelated thread"),
        /*response_handling*/ None,
    )
    .await;
    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"],
        "Core must validate ownership before retaining a process-local queue item"
    );
    assert!(!app.queued_agent_prompts.contains_key(&unrelated_thread_id));

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn direct_agent_prompt_materializes_a_known_alias_missing_from_navigation() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    app.agent_navigation
        .replace_aliases(vec![codex_app_server_protocol::AgentAlias {
            thread_id: target_thread_id.to_string(),
            agent_ref: "2".to_string(),
            nickname: Some("Robie".to_string()),
            state: codex_app_server_protocol::AgentAliasState::Active,
        }]);
    assert_eq!(app.agent_navigation.get(&target_thread_id), None);
    requests.lock().expect("request recorder lock").clear();

    app.submit_agent_prompt(
        &mut app_server,
        target_thread_id,
        crate::chatwidget::UserMessage::from("materialize and prompt this known agent"),
    )
    .await;

    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    assert!(app.agent_navigation.get(&target_thread_id).is_some());

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn interrupt_without_follow_up_refreshes_target_liveness() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let model_server = MockServer::start().await;
    mount_delayed_agent_prompt_response(&model_server, "interrupt-target-response").await;
    configure_agent_prompt_model_server(&mut app, &model_server);
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let source_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    display_test_thread(&mut app, source_thread_id);
    let target_thread_id = app
        .spawn_agent_from_command(
            &mut app_server,
            crate::app::SpawnAgentCommandArgs {
                source_thread_id,
                role: None,
                authored_selector: Some("new".to_string()),
                prompt: Some(crate::chatwidget::UserMessage::from(
                    "keep running until interrupted",
                )),
                fork_mode: codex_app_server_protocol::AgentForkMode::None,
                response_handling: None,
            },
        )
        .await
        .expect("spawned target");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !app.agent_navigation.is_running(target_thread_id)
        && tokio::time::Instant::now() < deadline
    {
        app.refresh_agent_picker_thread_liveness(&mut app_server, target_thread_id)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(app.agent_navigation.is_running(target_thread_id));
    requests.lock().expect("request recorder lock").clear();

    app.interrupt_agent_from_selector(
        &mut app_server,
        source_thread_id,
        crate::chatwidget::agent_command::AgentSelector {
            kind: crate::chatwidget::agent_command::AgentSelectorKind::Id(target_thread_id),
            authored: target_thread_id.to_string(),
        },
        /*follow_up*/ None,
        /*response_handling*/ None,
    )
    .await;

    assert!(
        !app.agent_navigation.is_running(target_thread_id),
        "pure interrupt should refresh the target instead of waiting for a listener event"
    );
    assert_eq!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["agent/control", "thread/read"]
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn queue_command_admitted_to_idle_agent_keeps_queued_prompt_provenance() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let displayed_thread_id = ThreadId::new();
    display_test_thread(&mut app, displayed_thread_id);
    let target_thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    app.upsert_agent_picker_thread(
        target_thread_id,
        Some("Robie".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    requests.lock().expect("request recorder lock").clear();
    let authored_selector = format!("id:{target_thread_id}");

    app.queue_agent_prompt_to_selector(
        &mut app_server,
        displayed_thread_id,
        crate::chatwidget::agent_command::AgentSelector {
            kind: crate::chatwidget::agent_command::AgentSelectorKind::Id(target_thread_id),
            authored: authored_selector.clone(),
        },
        crate::chatwidget::UserMessage::from("queue provenance"),
        /*response_handling*/ None,
    )
    .await;

    let recorded = requests.lock().expect("request recorder lock").clone();
    assert_eq!(
        recorded
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["thread/read", "agent/control", "thread/read"]
    );
    let params = recorded
        .iter()
        .find(|request| request.method == "agent/control")
        .and_then(|request| request.params.as_ref())
        .expect("queued agent/control params");
    assert_eq!(params["authoredSelector"], authored_selector);
    assert_eq!(params["action"]["type"], "queuedPrompt");

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn queued_prompt_edit_deletes_remote_entry_before_restoring_user_input() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let primary_thread_id = ThreadId::new();
    let target_thread_id = ThreadId::new();
    let prompt_id =
        uuid::Uuid::parse_str("00000000-0000-7000-8000-000000000001").expect("valid queue id");
    app.primary_thread_id = Some(primary_thread_id);
    display_test_thread(&mut app, primary_thread_id);
    app.apply_primary_agent_queue(vec![codex_app_server_protocol::AgentQueueEntry {
        id: prompt_id.to_string(),
        source_thread_id: primary_thread_id.to_string(),
        target_thread_id: target_thread_id.to_string(),
        input: vec![AppServerUserInput::Text {
            text: "review the queue lifecycle".to_string(),
            text_elements: Vec::new(),
        }],
        prompt_preview: "review the queue lifecycle".to_string(),
        response_handling: codex_app_server_protocol::AgentResponseHandling::new(
            /*commentary*/ false,
            codex_app_server_protocol::AgentFinalResponseHandling::Wake,
            /*target_messages*/ false,
            /*queue_input*/ true,
        ),
        authored_selector: Some("2".to_string()),
    }]);
    requests.lock().expect("request recorder lock").clear();

    app.edit_queued_agent_prompt(&app_server, target_thread_id, prompt_id)
        .await;

    let recorded = requests.lock().expect("request recorder lock").clone();
    assert_eq!(
        recorded
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["agentQueue/delete"]
    );
    assert_eq!(
        recorded[0].params.as_ref(),
        Some(&serde_json::json!({
            "rootThreadId": primary_thread_id,
            "id": prompt_id,
        }))
    );
    assert!(!app.queued_agent_prompts.contains_key(&target_thread_id));
    let restored = app.chat_widget.composer_text_with_pending();
    assert!(restored.starts_with("/agent queue 2 w:fq "));
    assert!(restored.contains("review the queue lifecycle"));

    let model_prompt_id =
        uuid::Uuid::parse_str("00000000-0000-7000-8000-000000000002").expect("valid queue id");
    app.apply_primary_agent_queue(vec![codex_app_server_protocol::AgentQueueEntry {
        id: model_prompt_id.to_string(),
        source_thread_id: primary_thread_id.to_string(),
        target_thread_id: target_thread_id.to_string(),
        input: vec![AppServerUserInput::Text {
            text: "model-authored queue entry".to_string(),
            text_elements: Vec::new(),
        }],
        prompt_preview: "model-authored queue entry".to_string(),
        response_handling: codex_app_server_protocol::AgentResponseHandling::new(
            /*commentary*/ false,
            codex_app_server_protocol::AgentFinalResponseHandling::Passive,
            /*target_messages*/ false,
            /*queue_input*/ true,
        ),
        authored_selector: None,
    }]);
    requests.lock().expect("request recorder lock").clear();

    app.edit_queued_agent_prompt(&app_server, target_thread_id, model_prompt_id)
        .await;

    assert!(
        requests.lock().expect("request recorder lock").is_empty(),
        "model-authored entries should reject editing before remote mutation"
    );
    assert_eq!(
        app.queued_agent_prompts
            .get(&target_thread_id)
            .map(VecDeque::len),
        Some(1)
    );
    assert_eq!(app.chat_widget.composer_text_with_pending(), restored);

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

fn create_history_rollout(
    config: &Config,
    history_mode: ThreadHistoryMode,
    preview: &str,
) -> Result<ThreadId> {
    let create_rollout = match history_mode {
        ThreadHistoryMode::Legacy => create_fake_rollout,
        ThreadHistoryMode::Paginated => create_fake_paginated_rollout,
    };
    let thread_id = create_rollout(
        config.codex_home.as_path(),
        "2026-01-02T00-00-00",
        "2026-01-02T00:00:00Z",
        preview,
        Some(config.model_provider_id.as_str()),
        /*git_info*/ None,
    )
    .map_err(|err| color_eyre::eyre::eyre!("failed to create history rollout: {err}"))?;
    Ok(ThreadId::from_string(&thread_id)?)
}

pub(super) fn recorded_params(requests: &RecordedRequests, method: &str) -> Vec<serde_json::Value> {
    requests
        .lock()
        .expect("request recorder lock")
        .iter()
        .filter(|request| request.method == method)
        .map(|request| request.params.clone().unwrap_or(serde_json::Value::Null))
        .collect()
}

async fn make_history_test_app() -> Result<(App, tempfile::TempDir)> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    Ok((app, codex_home))
}

#[tokio::test]
async fn removing_remote_thread_omits_disconnect_guidance() -> Result<()> {
    for event in [
        AppEvent::ArchiveCurrentThread,
        AppEvent::DeleteCurrentThread,
    ] {
        let (mut app, codex_home) = make_history_test_app().await?;
        let thread_id = ThreadId::from_string(
            &create_fake_rollout(
                codex_home.path(),
                "2026-01-01T00-00-00",
                "2026-01-01T00:00:00Z",
                "Saved user message",
                Some(app.config.model_provider_id.as_str()),
                /*git_info*/ None,
            )
            .expect("create rollout"),
        )?;
        let (mut server, _, proxy) = start_recording_app_server(
            &app.config,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
        )
        .await?;
        let resumed = server
            .resume_thread(
                app.config.clone(),
                thread_id,
                crate::app_server_session::ResumeModelSettings::RestoreFromThread,
            )
            .await?;
        app.app_server_target = AppServerTarget::Remote {
            endpoint: crate::resolve_remote_addr("ws://127.0.0.1:4500")?,
        };
        app.active_thread_id = Some(thread_id);
        app.chat_widget.handle_thread_session(resumed.session);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let archived = matches!(&event, AppEvent::ArchiveCurrentThread);
        let AppRunControl::Exit(reason) = app.handle_event(&mut tui, &mut server, event).await?
        else {
            panic!("removing the current thread must exit");
        };
        if archived {
            assert_matches!(reason, ExitReason::Archived(id) if id == thread_id);
        } else {
            assert_matches!(reason, ExitReason::ThreadRemoved);
        }
        let mut exit_info = app.exit_info(reason);
        exit_info.token_usage = TokenUsage {
            output_tokens: 2,
            total_tokens: 2,
            ..Default::default()
        };
        let mut expected = vec!["Token usage: total=2 input=0 output=2".to_string()];
        if archived {
            expected.push(format!("Session archived: {thread_id}"));
        }
        assert_eq!(
            exit_info.format_exit_messages(/*color_enabled*/ false),
            expected
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

fn spawn_approved_task_tool_call(
    app: &App,
    app_server: &AppServerSession,
    request_id: AppServerRequestId,
    params: codex_app_server_protocol::DynamicToolCallParams,
) {
    let request_handle = app_server.request_handle();
    let app_event_tx = app.app_event_tx.clone();
    let status_updates = app.dynamic_tool_status_updates.subscribe();
    let mut thread_start_params = crate::app_server_session::thread_start_params_from_config(
        &app.config,
        app_server.thread_params_mode(),
        app_server.remote_cwd_override(),
        /*session_start_source*/ None,
    );
    app_server
        .thread_tool_transport()
        .configure(&mut thread_start_params);
    tokio::spawn(async move {
        let response = crate::dynamic_tools::execute(
            request_handle,
            params,
            thread_start_params,
            status_updates,
            Some(&app_event_tx),
        )
        .await;
        app_event_tx.send(AppEvent::DynamicToolCallCompleted {
            request_id,
            response,
        });
    });
}

#[tokio::test]
async fn external_transport_registers_dynamic_tools_and_finds_task_mentions() -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;

    let started = app_server.start_thread(&app.config).await?;
    assert!(started.task_tools_available);
    assert!(app_server.task_tools_available(started.session.thread_id));
    let startup = crate::app_server_session::start_thread_with_request_handle(
        app_server.request_handle(),
        app.config.clone(),
        crate::app_server_session::ThreadParamsMode::Embedded,
        /*remote_cwd_override*/ None,
        app_server.thread_tool_transport(),
    )
    .await?;
    assert!(startup.task_tools_available);

    let starts = recorded_params(&requests, "thread/start");
    assert_eq!(starts.len(), 2);
    for params in starts {
        assert_eq!(params["dynamicTools"][0]["type"], "namespace");
        assert_eq!(params["dynamicTools"][0]["name"], "codex_tui");
        assert_eq!(
            params["dynamicTools"][0]["tools"].as_array().map(Vec::len),
            Some(6)
        );
        assert!(
            params["dynamicTools"][0]["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().all(|tool| {
                    tool["deferLoading"] == true
                        && !crate::dynamic_tools::DELEGATION_TOOLS
                            .contains(&tool["name"].as_str().unwrap_or_default())
                }))
        );
    }
    let target_id = started.session.thread_id;
    app_server
        .thread_inject_items(target_id, vec![App::side_boundary_prompt_item()])
        .await?;
    crate::init_state_db_for_app_server_target(&app.config, &crate::AppServerTarget::Embedded)
        .await?
        .expect("state database")
        .set_thread_preview_if_empty(target_id, "Review database migration")
        .await
        .expect("seed searchable thread preview");
    app_server.shutdown().await?;
    proxy.await??;
    let (mut restarted_app_server, _restarted_requests, restarted_proxy) =
        start_recording_app_server(
            &app.config,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
        )
        .await?;
    let resumed = restarted_app_server
        .resume_thread(
            app.config.clone(),
            target_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    assert!(resumed.task_tools_available);
    assert!(restarted_app_server.task_tools_available(target_id));
    let forked = restarted_app_server
        .fork_thread(
            app.config.clone(),
            target_id,
            /*source_rollout_path*/ None,
        )
        .await?;
    assert!(forked.task_tools_available);
    assert!(restarted_app_server.task_tools_available(forked.session.thread_id));
    restarted_app_server
        .thread_set_name(target_id, "Bluebird".to_string())
        .await?;
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();

    crate::task_mentions::spawn_search(
        restarted_app_server.request_handle(),
        "bbd".to_string(),
        startup.session.thread_id,
        app.config.cwd.to_path_buf(),
        restarted_app_server.task_search_generation(),
        crate::app_event_sender::AppEventSender::new(sender),
    );

    let event = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), events.recv())
        .await?
        .expect("expected task search results");
    let AppEvent::TaskSearchResult { matches, .. } = event else {
        panic!("expected task search results");
    };
    assert!(
        matches
            .iter()
            .any(|task| task.thread_id == target_id.to_string() && task.title == "Bluebird"),
        "expected created task in {matches:?}"
    );

    restarted_app_server.shutdown().await?;
    restarted_proxy.await??;
    Ok(())
}

#[tokio::test]
async fn archive_current_thread_reports_success_only_after_archiving() -> Result<()> {
    let (mut app, _codex_home) = make_history_test_app().await?;
    let thread_id = ThreadId::from_string(
        &create_fake_rollout(
            &app.config.codex_home,
            "2026-08-25T01-00-00",
            "2026-08-25T01:00:00Z",
            "archive me",
            Some(&app.config.model_provider_id),
            /*git_info*/ None,
        )
        .expect("create rollout"),
    )?;
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;

    app.active_thread_id = Some(ThreadId::new());
    assert_matches!(
        app.archive_current_thread(&mut app_server).await,
        AppRunControl::Continue
    );

    app.active_thread_id = Some(thread_id);
    assert_matches!(
        app.archive_current_thread(&mut app_server).await,
        AppRunControl::Exit(ExitReason::Archived(archived_id)) if archived_id == thread_id
    );

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn local_daemon_registers_approval_gated_mcp_tools_for_both_start_paths() -> Result<()> {
    let (mut app, events, _ops) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    app.config
        .web_search_mode
        .set(codex_protocol::config_types::WebSearchMode::Live)?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "web_search = \"disabled\"\n",
    )?;
    let (mut app_server, mut requests, mut proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    app_server
        .start_dynamic_tool_mcp(
            app.config.clone(),
            app.app_event_tx.clone(),
            app.dynamic_tool_status_updates.clone(),
        )
        .await?;

    let started = app_server.start_thread(&app.config).await?;
    let thread_id = started.session.thread_id;
    assert!(started.task_tools_available);
    assert!(app_server.task_tools_available(thread_id));
    let startup = crate::app_server_session::start_thread_with_request_handle(
        app_server.request_handle(),
        app.config.clone(),
        crate::app_server_session::ThreadParamsMode::Embedded,
        /*remote_cwd_override*/ None,
        app_server.thread_tool_transport(),
    )
    .await?;
    assert!(startup.task_tools_available);

    let inventory: codex_app_server_protocol::ListMcpServerStatusResponse = app_server
        .request_handle()
        .request_typed(ClientRequest::McpServerStatusList {
            request_id: AppServerRequestId::String("tui-tool-inventory".to_string()),
            params: codex_app_server_protocol::ListMcpServerStatusParams {
                cursor: None,
                limit: None,
                detail: Some(codex_app_server_protocol::McpServerStatusDetail::ToolsAndAuthOnly),
                thread_id: Some(thread_id.to_string()),
            },
        })
        .await?;
    let tools = &inventory
        .data
        .iter()
        .find(|server| server.name == "codex_tui")
        .expect("local daemon must connect to the TUI MCP server")
        .tools;
    assert_eq!(tools.len(), 9);
    for tool in crate::dynamic_tools::DELEGATION_TOOLS {
        assert!(tools.contains_key(tool));
    }
    assert!(
        !tools["create_thread"]
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|properties| properties.contains_key("permissions"))
    );

    let starts = recorded_params(&requests, "thread/start");
    assert_eq!(starts.len(), 2);
    for params in &starts {
        assert_eq!(params["dynamicTools"], serde_json::Value::Null);
        assert_eq!(params["config"]["web_search"], "live");
        let server = &params["config"]["mcp_servers.codex_tui"];
        assert!(
            server["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("http://127.0.0.1:"))
        );
        assert!(
            server["http_headers"]["Authorization"]
                .as_str()
                .is_some_and(|header| header.starts_with("Bearer "))
        );
        assert_eq!(server["default_tools_approval_mode"], "approve");
        for tool in crate::dynamic_tools::DELEGATION_TOOLS {
            assert_eq!(server["tools"][tool]["approval_mode"], "prompt");
        }
    }

    let mcp_url = starts[0]["config"]["mcp_servers.codex_tui"]["url"]
        .as_str()
        .expect("MCP server URL");
    let unauthorized = codex_http_client::HttpClientBuilder::new()
        .build_direct()?
        .post(mcp_url)
        .send()
        .await?;
    assert_eq!(unauthorized.status().as_u16(), 401);

    app.config
        .web_search_mode
        .set(codex_protocol::config_types::WebSearchMode::Disabled)?;
    let delegation_source = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Legacy,
        "Approved task source",
    )?;
    app_server
        .resume_thread(
            app.config.clone(),
            delegation_source,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let resumed = recorded_params(&requests, "thread/resume")
        .pop()
        .expect("resumed task request");
    assert_eq!(
        resumed["config"]["mcp_servers.codex_tui"],
        starts[0]["config"]["mcp_servers.codex_tui"]
    );
    app_server
        .resume_thread(
            app.config.clone(),
            delegation_source,
            crate::app_server_session::ResumeModelSettings::PreserveExistingThread,
        )
        .await?;
    let reattached = recorded_params(&requests, "thread/resume")
        .pop()
        .expect("reattached task request");
    assert_eq!(
        reattached["config"]["mcp_servers.codex_tui"],
        starts[0]["config"]["mcp_servers.codex_tui"]
    );
    app_server
        .fork_thread(
            app.config.clone(),
            delegation_source,
            /*source_rollout_path*/ None,
        )
        .await?;
    let forked = recorded_params(&requests, "thread/fork")
        .pop()
        .expect("forked task request");
    assert_eq!(
        forked["config"]["mcp_servers.codex_tui"],
        starts[0]["config"]["mcp_servers.codex_tui"]
    );
    let authorization =
        starts[0]["config"]["mcp_servers.codex_tui"]["http_headers"]["Authorization"]
            .as_str()
            .expect("MCP bearer token");
    let client = codex_http_client::HttpClientBuilder::new().build_direct()?;
    let call_tool = |id: u32, tool: &'static str, arguments: serde_json::Value| {
        client
            .post(mcp_url)
            .header("Authorization", authorization)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("MCP-Method", "tools/call")
            .header("MCP-Name", tool)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": arguments,
                    "_meta": {
                        "threadId": delegation_source,
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            }))
    };
    let transport = app_server.thread_tool_transport();
    let crate::dynamic_tools_mcp::ThreadToolTransport::Mcp(tool_server) = &transport else {
        panic!("expected the daemon task-tool bridge");
    };
    tool_server.suspend();
    let paused = call_tool(0, "list_threads", serde_json::json!({}))
        .send()
        .await?
        .text()
        .await?;
    assert!(
        paused.contains("TUI is reconnecting; tool was not sent"),
        "{paused}"
    );
    let (replacement, replacement_requests, replacement_proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let (new_tx, new_rx) = mpsc::unbounded_channel();
    let new_sender = AppEventSender::new(new_tx);
    drop(events);
    let mut events = new_rx;
    assert!(app.app_event_tx.app_event_tx.is_closed());
    tool_server.reconnect(replacement.request_handle(), new_sender);
    let previous = std::mem::replace(
        &mut app_server,
        replacement.with_thread_tool_transport(transport),
    );
    previous.shutdown().await?;
    proxy.await??;
    requests = replacement_requests;
    proxy = replacement_proxy;
    // The same MCP URL and credentials now use the new connection and event receiver.
    let response = call_tool(1, "list_threads", serde_json::json!({}))
        .send()
        .await?;
    let status = response.status();
    let response = response.text().await?;
    assert!(status.is_success(), "{status}: {response}");
    assert!(response.contains("threads"), "{response}");

    let mut creation = tokio::spawn(
        call_tool(
            2,
            "create_thread",
            serde_json::json!({"prompt": "Start an approved task"}),
        )
        .send(),
    );
    let registration = tokio::select! {
        event = events.recv() => event.expect("approved MCP task must register before starting"),
        response = &mut creation => {
            let response = response??;
            panic!("MCP task creation completed without registration: {}", response.text().await?);
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(/*secs*/ 5)) => {
            panic!("timed out waiting for MCP task registration");
        }
    };
    let AppEvent::DynamicToolThreadStarted {
        thread_id: child_thread_id,
        task_tools_available,
        registered,
    } = registration
    else {
        panic!("expected the MCP-created task to register")
    };
    assert!(task_tools_available);
    assert!(registered.send(()).is_ok());
    let created = creation.await??;
    assert!(created.status().is_success());
    assert!(created.text().await?.contains(&child_thread_id.to_string()));
    let child = recorded_params(&requests, "thread/start")
        .pop()
        .expect("MCP child thread/start request");
    assert_eq!(child["dynamicTools"], serde_json::Value::Null);
    assert!(child["config"]["web_search"].is_null());
    assert_eq!(
        child["config"]["mcp_servers.codex_tui"],
        starts[0]["config"]["mcp_servers.codex_tui"]
    );
    let forked = call_tool(
        3,
        "fork_thread",
        serde_json::json!({"threadId": delegation_source}),
    )
    .send()
    .await?;
    assert!(forked.status().is_success());
    let forked = recorded_params(&requests, "thread/fork")
        .pop()
        .expect("MCP-created fork request");
    assert_eq!(
        forked["config"]["mcp_servers.codex_tui"],
        starts[0]["config"]["mcp_servers.codex_tui"]
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn local_mcp_respects_configured_servers_and_managed_requirements() -> Result<()> {
    for scenario in ["conflicting", "blocked", "mismatched", "allowed"] {
        let (mut app, _codex_home) = make_history_test_app().await?;
        if scenario == "conflicting" {
            let raw = serde_json::from_value::<codex_config::RawMcpServerConfig>(
                serde_json::json!({"url": "http://127.0.0.1:1/mcp", "enabled": false}),
            )?;
            let mut servers = app.config.mcp_servers.get().clone();
            servers.insert(
                crate::dynamic_tools::NAMESPACE.to_string(),
                codex_config::McpServerConfig::try_from(raw)
                    .map_err(color_eyre::eyre::Report::msg)?,
            );
            app.config.mcp_servers.set(servers)?;
        } else {
            let mut allowed_servers = std::collections::BTreeMap::new();
            if matches!(scenario, "mismatched" | "allowed") {
                let requirement = if scenario == "allowed" {
                    codex_config::McpServerRequirement::Url(
                        codex_protocol::mcp_policy::McpServerValueMatcher::Prefix {
                            value: "http://127.0.0.1:".to_string(),
                        },
                    )
                } else {
                    codex_config::McpServerRequirement::Identity {
                        identity: codex_config::McpServerIdentity::Url {
                            url: "http://127.0.0.1:1/mcp".to_string(),
                        },
                    }
                };
                allowed_servers.insert(crate::dynamic_tools::NAMESPACE.to_string(), requirement);
            }
            let requirements = codex_config::ConfigRequirements {
                mcp_servers: Some(codex_config::Sourced::new(
                    allowed_servers,
                    codex_config::RequirementSource::Unknown,
                )),
                ..Default::default()
            };
            app.config.config_layer_stack = codex_config::ConfigLayerStack::new(
                Vec::new(),
                requirements,
                codex_config::ConfigRequirementsToml::default(),
            )?;
        }
        let (mut app_server, requests, proxy) = start_recording_app_server(
            &app.config,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
        )
        .await?;
        let result = app_server
            .start_dynamic_tool_mcp(
                app.config.clone(),
                app.app_event_tx.clone(),
                app.dynamic_tool_status_updates.clone(),
            )
            .await;
        if scenario == "allowed" {
            result?;
        } else {
            let error = result.expect_err("unavailable internal MCP must fail closed");
            assert_eq!(
                error.kind(),
                if scenario == "conflicting" {
                    std::io::ErrorKind::AlreadyExists
                } else {
                    std::io::ErrorKind::PermissionDenied
                }
            );
        }
        app_server.start_thread(&app.config).await?;
        let start = recorded_params(&requests, "thread/start")
            .pop()
            .expect("fallback task start");
        if scenario == "allowed" {
            assert!(start["dynamicTools"].is_null());
            assert!(start["config"]["mcp_servers.codex_tui"].is_object());
        } else {
            assert_eq!(
                start["dynamicTools"][0]["tools"].as_array().map(Vec::len),
                Some(6)
            );
            assert!(start["config"]["mcp_servers.codex_tui"].is_null());
        }
        app_server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn older_external_server_starts_without_unsupported_dynamic_tools_or_history() -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::LegacyDynamicToolsAndHistory,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;

    let started = app_server.start_thread(&app.config).await?;
    assert!(!started.task_tools_available);
    assert!(!app_server.task_tools_available(started.session.thread_id));
    let startup = crate::app_server_session::start_thread_with_request_handle(
        app_server.request_handle(),
        app.config.clone(),
        crate::app_server_session::ThreadParamsMode::Embedded,
        /*remote_cwd_override*/ None,
        app_server.thread_tool_transport(),
    )
    .await?;
    assert!(!startup.task_tools_available);

    let starts = recorded_params(&requests, "thread/start");
    assert_eq!(starts.len(), 4);
    for attempts in starts.chunks_exact(2) {
        assert_eq!(attempts[0]["dynamicTools"][0]["type"], "namespace");
        assert_eq!(attempts[0]["historyMode"], "legacy");
        assert_eq!(attempts[1]["dynamicTools"], serde_json::Value::Null);
        assert_eq!(attempts[1]["historyMode"], "legacy");
    }

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn embedded_server_rejects_unowned_dynamic_tool_calls() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerRequest(Box::new(
            ServerRequest::DynamicToolCall {
                request_id: AppServerRequestId::Integer(100),
                params: codex_app_server_protocol::DynamicToolCallParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-1".to_string(),
                    namespace: Some("codex_app".to_string()),
                    tool: "list_threads".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        )),
    )
    .await;
    let AppEvent::DynamicToolCallCompleted { response, .. } = events
        .try_recv()
        .expect("embedded dynamic calls must receive a response")
    else {
        panic!("expected a dynamic tool failure response")
    };
    assert!(!response.success);
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn dynamic_tool_requests_ignore_other_namespaces_and_dispatch_tui_namespace() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    app.config
        .permissions
        .set_permission_profile(PermissionProfile::workspace_write_with(
            &[app.config.cwd.clone()],
            codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ))?;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ Some("Unavailable name"),
    )
    .await?;
    let thread_id = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id
        .to_string();

    for namespace in [Some("codex_app"), None] {
        app.handle_app_server_event(
            &app_server,
            codex_app_server_client::AppServerEvent::ServerRequest(Box::new(
                ServerRequest::DynamicToolCall {
                    request_id: AppServerRequestId::Integer(100),
                    params: codex_app_server_protocol::DynamicToolCallParams {
                        thread_id: thread_id.clone(),
                        turn_id: "turn-1".to_string(),
                        call_id: "call-1".to_string(),
                        namespace: namespace.map(str::to_string),
                        tool: "list_threads".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            )),
        )
        .await;
        assert!(events.try_recv().is_err());
    }

    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerRequest(Box::new(
            ServerRequest::DynamicToolCall {
                request_id: AppServerRequestId::Integer(101),
                params: codex_app_server_protocol::DynamicToolCallParams {
                    thread_id: thread_id.clone(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-2".to_string(),
                    namespace: Some("codex_tui".to_string()),
                    tool: "list_threads".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        )),
    )
    .await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), events.recv())
        .await?
        .expect("dynamic tool completion event");
    let AppEvent::DynamicToolCallCompleted {
        request_id,
        response,
    } = event
    else {
        panic!("expected a dynamic tool completion event")
    };
    assert_eq!(request_id, AppServerRequestId::Integer(101));
    assert!(response.success, "{response:?}");
    let list_requests = recorded_params(&requests, "thread/list");
    assert_eq!(list_requests.len(), 1);
    assert_eq!(list_requests[0]["useStateDbOnly"], true);
    assert_eq!(list_requests[0]["sourceKinds"], serde_json::Value::Null);

    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::DynamicToolCallCompleted {
            request_id,
            response,
        },
    )
    .await?;
    let completed = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
        loop {
            if let Some(response) = recorded_params(&requests, "server/request/response").pop() {
                break response;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(completed["success"], true);

    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerRequest(Box::new(
            ServerRequest::DynamicToolCall {
                request_id: AppServerRequestId::Integer(102),
                params: codex_app_server_protocol::DynamicToolCallParams {
                    thread_id: thread_id.clone(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-3".to_string(),
                    namespace: Some("codex_tui".to_string()),
                    tool: "set_thread_title".to_string(),
                    arguments: serde_json::json!({"threadId": thread_id, "title": "Renamed"}),
                },
            },
        )),
    )
    .await;
    let AppEvent::DynamicToolCallCompleted { response, .. } =
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), events.recv())
            .await?
            .expect("dynamic mutation completion event")
    else {
        panic!("expected a dynamic mutation completion event")
    };
    assert!(response.success, "{response:?}");
    assert_eq!(
        recorded_params(&requests, "thread/name/set")[0]["name"],
        "Renamed"
    );

    for (index, tool) in crate::dynamic_tools::DELEGATION_TOOLS
        .into_iter()
        .enumerate()
    {
        app.handle_app_server_event(
            &app_server,
            codex_app_server_client::AppServerEvent::ServerRequest(Box::new(
                ServerRequest::DynamicToolCall {
                    request_id: AppServerRequestId::String(format!("rejected-{index}")),
                    params: codex_app_server_protocol::DynamicToolCallParams {
                        thread_id: thread_id.clone(),
                        turn_id: "turn-1".to_string(),
                        call_id: format!("rejected-{index}"),
                        namespace: Some("codex_tui".to_string()),
                        tool: tool.to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            )),
        )
        .await;
        let AppEvent::DynamicToolCallCompleted { response, .. } = events
            .try_recv()
            .expect("legacy delegation call must receive an immediate rejection")
        else {
            panic!("expected a legacy delegation failure response")
        };
        assert!(!response.success);
    }

    let creation_source = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Legacy,
        "Background task source",
    )?;
    app_server
        .resume_thread(
            app.config.clone(),
            creation_source,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let project: codex_app_server_protocol::ProjectCreateResponse = app_server
        .request_handle()
        .request_typed(ClientRequest::ProjectCreate {
            request_id: AppServerRequestId::String("create-source-project".to_string()),
            params: codex_app_server_protocol::ProjectCreateParams {
                name: "Source project".to_string(),
                roots: vec![codex_app_server_protocol::ProjectRoot {
                    path: app.config.cwd.clone(),
                }],
                metadata: None,
                idempotency_key: "source-project".to_string(),
            },
        })
        .await?;
    let _: codex_app_server_protocol::ThreadMetadataUpdateResponse = app_server
        .request_handle()
        .request_typed(ClientRequest::ThreadMetadataUpdate {
            request_id: AppServerRequestId::String("assign-source-project".to_string()),
            params: codex_app_server_protocol::ThreadMetadataUpdateParams {
                thread_id: creation_source.to_string(),
                project_id: Some(project.project.id.clone()),
                git_info: None,
            },
        })
        .await?;
    let source_settings: codex_app_server_protocol::ThreadResumeResponse = app_server
        .request_handle()
        .request_typed(ClientRequest::ThreadResume {
            request_id: AppServerRequestId::String("read-source-sandbox".to_string()),
            params: codex_app_server_protocol::ThreadResumeParams {
                thread_id: creation_source.to_string(),
                ..codex_app_server_protocol::ThreadResumeParams::default()
            },
        })
        .await?;
    assert!(source_settings.active_permission_profile.is_none());
    let source_sandbox = serde_json::to_value(source_settings.sandbox)?;
    spawn_approved_task_tool_call(
        &app,
        &app_server,
        AppServerRequestId::Integer(103),
        codex_app_server_protocol::DynamicToolCallParams {
            thread_id: creation_source.to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-4".to_string(),
            namespace: Some("codex_tui".to_string()),
            tool: "create_thread".to_string(),
            arguments: serde_json::json!({
                "prompt": "Check <main> & report",
                "title": "Unavailable name"
            }),
        },
    );
    let registration =
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), events.recv())
            .await?
            .expect("background task registration event");
    let AppEvent::DynamicToolThreadStarted {
        thread_id: created_thread_id,
        task_tools_available,
        registered,
    } = registration
    else {
        panic!("expected background task registration before its first turn: {registration:?}")
    };
    assert!(recorded_params(&requests, "turn/start").is_empty());
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::DynamicToolThreadStarted {
            thread_id: created_thread_id,
            task_tools_available,
            registered,
        },
    )
    .await?;
    assert!(
        app.agents_overview
            .dispatched_requests
            .contains_key(&created_thread_id)
    );
    assert!(app_server.task_tools_available(created_thread_id));
    let AppEvent::DynamicToolCallCompleted { response, .. } =
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), events.recv())
            .await?
            .expect("background task creation completion")
    else {
        panic!("expected a background task completion event")
    };
    assert!(response.success, "{response:?}");
    assert_eq!(
        recorded_params(&requests, "thread/start")
            .last()
            .expect("background task creation")["projectId"],
        project.project.id
    );
    let turn = recorded_params(&requests, "turn/start")
        .pop()
        .expect("background task turn request");
    assert_eq!(turn["input"], serde_json::json!([]));
    assert_eq!(
        turn["toolOutput"],
        serde_json::json!({
            "name": "create_thread",
            "namespace": "codex_tui",
            "output": format!(
                "<codex_delegation>\n  <source_thread_id>{creation_source}</source_thread_id>\n  <input>Check &lt;main&gt; &amp; report</input>\n</codex_delegation>"
            )
        })
    );
    assert_eq!(turn["sandboxPolicy"], source_sandbox);
    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerRequest(Box::new(exec_approval_request(
            created_thread_id,
            "turn-2",
            "item-1",
            /*approval_id*/ None,
        ))),
    )
    .await;
    assert_eq!(
        app.agents_overview.dispatched_requests[&created_thread_id].len(),
        1
    );

    spawn_approved_task_tool_call(
        &app,
        &app_server,
        AppServerRequestId::Integer(104),
        codex_app_server_protocol::DynamicToolCallParams {
            thread_id: thread_id.clone(),
            turn_id: "turn-1".to_string(),
            call_id: "call-5".to_string(),
            namespace: Some("codex_tui".to_string()),
            tool: "send_message_to_thread".to_string(),
            arguments: serde_json::json!({
                "threadId": creation_source,
                "prompt": "Follow <up> & report"
            }),
        },
    );
    let AppEvent::DynamicToolThreadStarted {
        thread_id: continued_thread_id,
        task_tools_available,
        registered,
    } = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), events.recv())
        .await?
        .expect("follow-up task registration event")
    else {
        panic!("expected follow-up task registration before its next turn")
    };
    assert_eq!(continued_thread_id, creation_source);
    assert_eq!(recorded_params(&requests, "turn/start").len(), 1);
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::DynamicToolThreadStarted {
            thread_id: continued_thread_id,
            task_tools_available,
            registered,
        },
    )
    .await?;
    let AppEvent::DynamicToolCallCompleted { response, .. } =
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), events.recv())
            .await?
            .expect("follow-up task completion")
    else {
        panic!("expected a follow-up task completion event")
    };
    assert!(response.success, "{response:?}");
    let turn = &recorded_params(&requests, "turn/start")[1];
    assert_eq!(turn["input"], serde_json::json!([]));
    assert_eq!(
        turn["toolOutput"],
        serde_json::json!({
            "name": "send_message_to_thread",
            "namespace": "codex_tui",
            "output": format!(
                "<codex_delegation>\n  <source_thread_id>{thread_id}</source_thread_id>\n  <input>Follow &lt;up&gt; &amp; report</input>\n</codex_delegation>"
            )
        })
    );

    app.dynamic_tool_tasks.insert(
        AppServerRequestId::Integer(105),
        (thread_id, tokio::spawn(std::future::pending::<()>())),
    );
    assert_matches!(
        app.handle_exit_mode(&mut app_server, ExitMode::ShutdownFirst)
            .await,
        AppRunControl::Exit(ExitReason::UserRequested)
    );
    let cancelled = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            if let Some(response) = recorded_params(&requests, "server/request/response")
                .into_iter()
                .find(|response| response["success"] == false)
            {
                break response;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(cancelled["success"], false);
    assert!(app.dynamic_tool_tasks.is_empty());

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn older_pagination_reconciles_review_prompts_across_page_boundaries() -> Result<()> {
    let (mut app, codex_home) = make_history_test_app().await?;
    app.config.terminal_resize_reflow.max_rows = TerminalResizeReflowMaxRows::Limit(100);
    let thread_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2026-01-02T00-00-00",
        "2026-01-02T00:00:00Z",
        "older visible prompt",
        Some(app.config.model_provider_id.as_str()),
        /*git_info*/ None,
    )
    .map_err(|error| color_eyre::eyre::eyre!("failed to create paginated rollout: {error}"))?;
    let thread_id = ThreadId::from_string(&thread_id)?;
    let path = rollout_path(
        codex_home.path(),
        "2026-01-02T00-00-00",
        &thread_id.to_string(),
    );
    let mut records = std::fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let user_item = |id: &str, text: &str| {
        TurnItem::UserMessage(UserMessageItem {
            id: id.to_string(),
            client_id: None,
            content: vec![CoreUserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
        })
    };
    let mut items = vec![
        user_item("older-visible-prompt", "older visible prompt"),
        TurnItem::EnteredReviewMode(EnteredReviewModeItem {
            id: "cross-page-review-start".to_string(),
            target: ReviewTarget::UncommittedChanges,
            user_facing_hint: "review started".to_string(),
        }),
        user_item("hidden-review-prompt", "hidden cross-page review prompt"),
    ];
    items.extend((0..97).map(|index| {
        TurnItem::AgentMessage(AgentMessageItem {
            id: format!("review-output-{index}"),
            content: vec![AgentMessageContent::Text {
                text: format!("review output {index}"),
            }],
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
            sub_agent_completion: None,
        })
    }));
    items.extend([
        TurnItem::ExitedReviewMode(ExitedReviewModeItem {
            id: "cross-page-review-end".to_string(),
            review_output: None,
        }),
        user_item("newer-visible-prompt", "newer visible prompt"),
    ]);
    let events = std::iter::once(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "cross-page-review-turn".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
        agent_queue: None,
    }))
    .chain(items.into_iter().map(|item| {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "cross-page-review-turn".to_string(),
            item,
            started_at_ms: None,
            completed_at_ms: 0,
        })
    }));
    for event in events {
        records.push(serde_json::json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "ordinal": records.len(),
            "type": "event_msg",
            "payload": serde_json::to_value(event)?,
        }));
    }
    let records = records
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{records}\n"))?;
    let (mut app_server, _requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let started = app_server
        .resume_thread(
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let initial_cells = test_transcript_cells(
        Some(thread_id),
        &app.config.cwd,
        started.turns.iter().flat_map(|turn| turn.items.clone()),
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    );
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.transcript_cells = initial_cells;
    assert_eq!(
        app.transcript_cells
            .iter()
            .filter_map(|cell| cell.as_any().downcast_ref::<UserHistoryCell>())
            .map(|user| user.message.as_str())
            .collect::<Vec<_>>(),
        vec!["hidden cross-page review prompt", "newer visible prompt"]
    );
    app.backtrack.overlay_preview_active = true;
    app.backtrack.nth_user_message = 1;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.open_transcript_overlay(&mut tui);
    app.apply_backtrack_selection_internal(app.backtrack.nth_user_message);
    let cursor = app_server
        .begin_older_history_page(thread_id)
        .expect("review-mode marker should remain on an older page");
    let request_id = app_server.next_request_id();
    let page: ThreadItemsListResponse = app_server
        .request_handle()
        .request_typed(ClientRequest::ThreadItemsList {
            request_id,
            params: ThreadItemsListParams {
                thread_id: thread_id.to_string(),
                turn_id: None,
                cursor: Some(cursor.clone()),
                limit: Some(crate::app_server_session::HISTORY_ITEM_PAGE_LIMIT),
                sort_direction: Some(SortDirection::Desc),
            },
        })
        .await?;
    app.handle_older_history_page(&mut tui, &mut app_server, thread_id, &cursor, Ok(page))
        .await?;

    let visible_user_messages = app
        .transcript_cells
        .iter()
        .filter_map(|cell| cell.as_any().downcast_ref::<UserHistoryCell>())
        .map(|user| user.message.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        visible_user_messages,
        vec!["older visible prompt", "newer visible prompt"]
    );
    assert_eq!(app.backtrack.nth_user_message, 1);
    let area = ratatui::layout::Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 12,
    );
    let mut buffer = Buffer::empty(area);
    let Some(Overlay::Transcript(overlay)) = app.overlay.as_mut() else {
        panic!("expected a transcript overlay");
    };
    overlay.render(area, &mut buffer);
    let highlighted_output = area
        .positions()
        .filter(|position| {
            buffer[*position]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        })
        .map(|position| buffer[position].symbol())
        .collect::<String>();
    let highlighted_message = visible_user_messages
        .iter()
        .find(|message| highlighted_output.contains(message.as_str()))
        .expect("the selected user message should remain highlighted");
    insta::assert_snapshot!(
        format!(
            "{}\nhighlighted: {highlighted_message}",
            visible_user_messages.join("\n")
        ),
        @r"
    older visible prompt
    newer visible prompt
    highlighted: newer visible prompt
    ");

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn transcript_home_loads_every_older_history_page() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    app.config.terminal_resize_reflow.max_rows = TerminalResizeReflowMaxRows::Limit(2);
    let thread_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2026-01-02T00-00-00",
        "2026-01-02T00:00:00Z",
        "multi-page transcript",
        Some(app.config.model_provider_id.as_str()),
        /*git_info*/ None,
    )
    .map_err(|error| color_eyre::eyre::eyre!("failed to create paginated rollout: {error}"))?;
    let thread_id = ThreadId::from_string(&thread_id)?;
    let path = rollout_path(
        codex_home.path(),
        "2026-01-02T00-00-00",
        &thread_id.to_string(),
    );
    let mut records = std::fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let child_thread_id = ThreadId::new();
    let events = std::iter::once(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "multi-page-turn".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
        agent_queue: None,
    }))
    .chain(std::iter::once(EventMsg::ItemCompleted(
        ItemCompletedEvent {
            thread_id,
            turn_id: "multi-page-turn".to_string(),
            item: TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: "multi-page-spawn".to_string(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::Completed,
                observe_commentary: Some(false),
                wake_on_completion: Some(false),
                target_messages: Some(false),
                queue_input: Some(false),
                deadline_at_ms: None,
                sender_thread_id: thread_id,
                receiver_thread_ids: vec![child_thread_id],
                receiver_agents: vec![CollabAgentRef {
                    thread_id: child_thread_id,
                    agent_nickname: Some("Robie".to_string()),
                    agent_role: Some("explorer".to_string()),
                }],
                prompt: Some("Inspect paginated history.".to_string()),
                model: None,
                reasoning_effort: None,
                agents_states: Default::default(),
                completion_presentation_agent_ids: None,
            }),
            started_at_ms: None,
            completed_at_ms: 0,
        },
    )))
    .chain((0..305).map(|index| {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "multi-page-turn".to_string(),
            item: TurnItem::AgentMessage(AgentMessageItem {
                id: format!("history-item-{index}"),
                content: vec![AgentMessageContent::Text {
                    text: format!("history output {index}"),
                }],
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
                sub_agent_completion: None,
            }),
            started_at_ms: None,
            completed_at_ms: 0,
        })
    }))
    .chain(std::iter::once(EventMsg::ItemCompleted(
        ItemCompletedEvent {
            thread_id,
            turn_id: "multi-page-turn".to_string(),
            item: TurnItem::AgentMessage(
                sub_agent_completion_item(
                    &child_thread_id.to_string(),
                    &AgentStatus::Completed(Some("Finished the paginated review.".to_string())),
                )
                .expect("terminal completion"),
            ),
            started_at_ms: None,
            completed_at_ms: 0,
        },
    )));
    for event in events {
        records.push(serde_json::json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "ordinal": records.len(),
            "type": "event_msg",
            "payload": serde_json::to_value(event)?,
        }));
    }
    let records = records
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{records}\n"))?;

    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let started = app_server
        .resume_thread(
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let initial_cells = test_transcript_cells(
        Some(thread_id),
        &app.config.cwd,
        started.turns.iter().flat_map(|turn| turn.items.clone()),
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    );
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.transcript_cells = initial_cells;
    while app_event_rx.try_recv().is_ok() {}
    let initial_turn_requests = recorded_params(&requests, "thread/turns/list").len();
    let initial_item_requests = recorded_params(&requests, "thread/items/list").len();
    let export_path = codex_home.path().join("complete-export.md");
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ true);
    app.chat_widget.insert_str("queued after export");
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::ExportTranscript {
            destination: TranscriptExportDestination::File(export_path.clone()),
        },
    )
    .await?;
    assert!(app.chat_widget.queued_user_message_texts().is_empty());
    let markdown = std::fs::read_to_string(export_path)?;
    assert!(
        (0..305)
            .map(|index| format!("history output {index}"))
            .eq(markdown.lines().filter(|line| line.starts_with("history")))
    );
    assert!(
        markdown.contains("Robie [explorer] completed (● visible):"),
        "{markdown}"
    );
    assert!(
        recorded_params(&requests, "thread/turns/list")[initial_turn_requests..]
            .iter()
            .all(|params| params["itemsView"] == "notLoaded")
    );
    assert!(
        recorded_params(&requests, "thread/items/list")[initial_item_requests..]
            .iter()
            .all(|params| {
                params["limit"] == crate::app_server_session::HISTORY_ITEM_PAGE_LIMIT
            })
    );
    app.scrollback_has_older_history = app_server.has_older_history(thread_id);
    assert!(app.scrollback_has_older_history);
    while app_event_rx.try_recv().is_ok() {}
    let initial_page_requests = recorded_params(&requests, "thread/items/list").len();
    app.open_transcript_overlay(&mut tui);

    app.handle_backtrack_overlay_event(
        &mut tui,
        &mut app_server,
        TuiEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
    )
    .await?;
    while app_server.has_older_history(thread_id) {
        let event = tokio::time::timeout(Duration::from_secs(5), app_event_rx.recv())
            .await?
            .ok_or_else(|| color_eyre::eyre::eyre!("history event channel closed"))?;
        if matches!(event, AppEvent::OlderThreadHistoryLoaded { .. }) {
            app.handle_event(&mut tui, &mut app_server, event).await?;
        }
    }

    assert!(recorded_params(&requests, "thread/items/list").len() >= initial_page_requests + 3);
    assert!(app.transcript_cells.iter().any(|cell| {
        cell.display_lines(/*width*/ 80)
            .iter()
            .any(|line| line.to_string().contains("history output 0"))
    }));
    assert!(app.transcript_cells.iter().any(|cell| {
        cell.display_lines(/*width*/ 80).iter().any(|line| {
            line.to_string()
                .contains("Robie [explorer] completed (● visible):")
        })
    }));
    let Some(Overlay::Transcript(overlay)) = app.overlay.as_mut() else {
        panic!("expected transcript overlay after Home navigation");
    };
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 12,
    );
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    let visible = (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible.contains("history output 0"), "{visible}");
    assert!(!visible.contains("history output 304"), "{visible}");
    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn remote_non_paginated_start_negotiates_on_resume_once_for_fork() -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let legacy_thread_id =
        create_history_rollout(&app.config, ThreadHistoryMode::Legacy, "legacy history")?;
    let paginated_thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "paginated history",
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::LegacyOnly,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;

    let started = app_server.start_thread(&app.config).await?;
    let resumed = app_server
        .resume_thread(
            app.config.clone(),
            legacy_thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let forked = app_server
        .fork_thread(
            app.config.clone(),
            legacy_thread_id,
            /*source_rollout_path*/ None,
        )
        .await?;

    assert_ne!(started.session.thread_id, legacy_thread_id);
    assert_eq!(resumed.session.thread_id, legacy_thread_id);
    assert_ne!(forked.session.thread_id, legacy_thread_id);
    let starts = recorded_params(&requests, "thread/start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["historyMode"], "legacy");

    let resumes = recorded_params(&requests, "thread/resume");
    assert_eq!(resumes.len(), 2);
    assert_eq!(resumes[0]["excludeTurns"], true);
    assert_ne!(resumes[1]["excludeTurns"], true);
    let forks = recorded_params(&requests, "thread/fork");
    assert_eq!(forks.len(), 1, "fork must reuse resume negotiation");
    assert_ne!(forks[0]["excludeTurns"], true);
    assert!(recorded_params(&requests, "thread/turns/list").is_empty());
    assert!(recorded_params(&requests, "thread/items/list").is_empty());

    let initial_read_count = recorded_params(&requests, "thread/read").len();
    let exported = crate::app::transcript_export::load_export_transcript(
        &mut app_server,
        paginated_thread_id,
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
        vec![Arc::new(PlainHistoryCell::new(vec!["visible".into()]))],
    )
    .await
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    assert_eq!(exported[0].raw_lines()[0].to_string(), "visible");
    assert!(
        recorded_params(&requests, "thread/read")[initial_read_count..]
            .iter()
            .any(|params| params["includeTurns"] == true)
    );
    assert_eq!(recorded_params(&requests, "thread/turns/list").len(), 1);

    let (_status_sender, status_updates) = tokio::sync::broadcast::channel(/*capacity*/ 1);
    let response = crate::dynamic_tools::execute(
        app_server.request_handle(),
        codex_app_server_protocol::DynamicToolCallParams {
            thread_id: started.session.thread_id.to_string(),
            turn_id: "source-turn".to_string(),
            call_id: "legacy-wait".to_string(),
            namespace: Some(crate::dynamic_tools::NAMESPACE.to_string()),
            tool: "wait_threads".to_string(),
            arguments: serde_json::json!({
                "targets": [{"threadId": legacy_thread_id}],
                "timeoutMs": 0
            }),
        },
        codex_app_server_protocol::ThreadStartParams::default(),
        status_updates,
        /*app_event_tx*/ None,
    )
    .await;
    assert!(response.success, "{response:?}");
    assert!(
        recorded_params(&requests, "thread/read")
            .iter()
            .any(|params| {
                params["threadId"] == legacy_thread_id.to_string() && params["includeTurns"] == true
            })
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn remote_legacy_history_start_avoids_unsupported_paginated_variant() -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::LegacyOnlyUnsupportedVariant,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;

    app_server.start_thread(&app.config).await?;

    let starts = recorded_params(&requests, "thread/start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["historyMode"], "legacy");

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyHistoryRequest {
    Resume,
    Fork,
}

async fn assert_remote_legacy_history_retry(request: LegacyHistoryRequest) -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let legacy_thread_id =
        create_history_rollout(&app.config, ThreadHistoryMode::Legacy, "legacy history")?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::LegacyOnly,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;

    let method = match request {
        LegacyHistoryRequest::Resume => {
            let resumed = app_server
                .resume_thread(
                    app.config.clone(),
                    legacy_thread_id,
                    crate::app_server_session::ResumeModelSettings::RestoreFromThread,
                )
                .await?;
            assert_eq!(resumed.session.thread_id, legacy_thread_id);
            "thread/resume"
        }
        LegacyHistoryRequest::Fork => {
            let forked = app_server
                .fork_thread(
                    app.config.clone(),
                    legacy_thread_id,
                    /*source_rollout_path*/ None,
                )
                .await?;
            assert_ne!(forked.session.thread_id, legacy_thread_id);
            "thread/fork"
        }
    };
    let attempts = recorded_params(&requests, method);
    if request == LegacyHistoryRequest::Resume {
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0]["excludeTurns"], true);
    } else {
        assert_eq!(attempts.len(), 1);
    }
    assert_ne!(
        attempts.last().expect("history request")["excludeTurns"],
        true
    );
    assert!(recorded_params(&requests, "thread/turns/list").is_empty());
    assert!(recorded_params(&requests, "thread/items/list").is_empty());

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn remote_legacy_history_resume_retries_generic_method_not_found() -> Result<()> {
    assert_remote_legacy_history_retry(LegacyHistoryRequest::Resume).await
}

#[tokio::test]
async fn remote_legacy_history_fork_avoids_unsupported_fields() -> Result<()> {
    assert_remote_legacy_history_retry(LegacyHistoryRequest::Fork).await
}

#[tokio::test]
async fn paginated_fork_survives_post_response_hydration_failure() -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let parent_thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "paginated fork parent",
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::ForkHydrationFails,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;

    let started = app_server
        .resume_thread(
            app.config.clone(),
            parent_thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    assert_eq!(started.session.thread_id, parent_thread_id);

    let forked = app_server
        .fork_thread(
            app.config.clone(),
            parent_thread_id,
            /*source_rollout_path*/ None,
        )
        .await?;

    assert_ne!(forked.session.thread_id, parent_thread_id);
    assert_eq!(recorded_params(&requests, "thread/fork").len(), 1);

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn underfilled_scrollback_fetches_older_pages_without_opening_the_transcript() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    app.config.terminal_resize_reflow.max_rows = TerminalResizeReflowMaxRows::Limit(8);
    let thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "scrollback pagination",
    )?;
    let path = rollout_path(
        codex_home.path(),
        "2026-01-02T00-00-00",
        &thread_id.to_string(),
    );
    let mut records = std::fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let events = std::iter::once(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "scrollback-pagination-turn".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
        agent_queue: None,
    }))
    .chain((0..120).map(|index| {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "scrollback-pagination-turn".to_string(),
            item: TurnItem::AgentMessage(AgentMessageItem {
                id: format!("scrollback-item-{index}"),
                content: vec![AgentMessageContent::Text {
                    text: format!("scrollback output {index}"),
                }],
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
                sub_agent_completion: None,
            }),
            started_at_ms: None,
            completed_at_ms: 0,
        })
    }));
    for event in events {
        records.push(serde_json::json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "ordinal": records.len(),
            "type": "event_msg",
            "payload": serde_json::to_value(event)?,
        }));
    }
    let records = records
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{records}\n"))?;

    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let started = app_server
        .resume_thread(
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let mut initial_cells = test_transcript_cells(
        Some(thread_id),
        &app.config.cwd,
        started.turns.iter().flat_map(|turn| turn.items.clone()),
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    );
    initial_cells.insert(
        /*index*/ 0,
        Arc::new(crate::history_cell::new_session_info(
            &app.config,
            started.session.model.as_str(),
            &started.session,
            /*is_first_event*/ false,
            Some("This is a test announcement".to_string()),
            /*auth_plan*/ None,
            /*show_fast_status*/ false,
        )),
    );
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.transcript_cells = initial_cells;
    app.scrollback_has_older_history = app_server.has_older_history(thread_id);
    app.config.terminal_resize_reflow.max_rows = TerminalResizeReflowMaxRows::Limit(32);
    let initial_cell_count = app.transcript_cells.len();
    let initial_page_requests = recorded_params(&requests, "thread/items/list").len();
    let mut tui = crate::tui::test_support::make_test_tui()?;

    app.scrollback_has_older_history = false;
    app.handle_key_event(
        &mut tui,
        &mut app_server,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    )
    .await;
    assert!(app.scrollback_has_older_history);
    if let Some(Overlay::Transcript(overlay)) = app.overlay.as_mut() {
        overlay.handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        )?;
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 100, /*height*/ 16,
        );
        let render_overlay = |overlay: &mut crate::pager_overlay::TranscriptOverlay| {
            let mut buffer = Buffer::empty(area);
            overlay.render(area, &mut buffer);
            (area.y..area.bottom())
                .map(|y| {
                    (area.x..area.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let partial = render_overlay(overlay);
        assert!(partial.contains("Earlier messages are available — scroll up to load them"));
        assert!(!partial.contains("OpenAI Codex"));
        assert!(!partial.contains("This is a test announcement"));
        assert!(!partial.contains('%'));

        overlay.set_history_state(crate::pager_overlay::TranscriptHistoryState::LoadingOlder);
        let loading = render_overlay(overlay);
        assert!(loading.contains("Loading earlier messages..."));
        assert!(!loading.contains("OpenAI Codex"));
        assert!(!loading.contains('%'));
    } else {
        panic!("expected transcript overlay");
    }
    app.close_transcript_overlay(&mut tui);

    let terminal_width = tui.terminal.last_known_screen_size.into();
    app.reflow_transcript_now(&mut tui, terminal_width)?;
    let request = loop {
        match app_event_rx.recv().await {
            Some(event @ AppEvent::RequestOlderScrollbackHistory { .. }) => break event,
            Some(_) => {}
            None => panic!("scrollback refill request channel closed"),
        }
    };
    app.handle_event(&mut tui, &mut app_server, request).await?;
    let loaded = loop {
        match app_event_rx.recv().await {
            Some(event @ AppEvent::OlderThreadHistoryLoaded { .. }) => break event,
            Some(_) => {}
            None => panic!("older history page channel closed"),
        }
    };
    app.handle_event(&mut tui, &mut app_server, loaded).await?;

    assert!(app.overlay.is_none());
    assert!(app.transcript_cells.len() > initial_cell_count);
    assert_eq!(
        recorded_params(&requests, "thread/items/list").len(),
        initial_page_requests + 1
    );
    assert_eq!(
        app.render_transcript_lines_for_reflow(/*width*/ 80)
            .lines
            .len(),
        32
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn paginated_workflows_never_request_full_thread_history() -> Result<()> {
    let (app, _codex_home) = make_history_test_app().await?;
    let paginated_thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "paginated visible history",
    )?;
    let legacy_thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Legacy,
        "legacy visible history",
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;

    app_server.remember_thread_history_mode(paginated_thread_id, ThreadHistoryMode::Legacy);
    let resumed = app_server
        .resume_thread(
            app.config.clone(),
            paginated_thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    assert_eq!(resumed.session.thread_id, paginated_thread_id);
    assert!(recorded_params(&requests, "thread/read").is_empty());
    let resume_requests = recorded_params(&requests, "thread/resume");
    assert_eq!(resume_requests.len(), 1);
    assert_eq!(resume_requests[0]["excludeTurns"], true);
    let cells = crate::thread_transcript::load_session_transcript(
        &mut app_server,
        paginated_thread_id,
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    )
    .await?;
    assert!(!cells.is_empty());
    app_server
        .fork_thread(
            app.config.clone(),
            paginated_thread_id,
            /*source_rollout_path*/ None,
        )
        .await?;
    let mut side_config = app.config.clone();
    side_config.ephemeral = true;
    app_server
        .fork_side_thread(side_config, paginated_thread_id)
        .await?;

    let paginated_reads = recorded_params(&requests, "thread/read");
    assert!(!paginated_reads.is_empty());
    assert!(
        paginated_reads
            .iter()
            .all(|params| params["includeTurns"] != true),
        "paginated workflows requested full history: {paginated_reads:?}"
    );
    assert!(!recorded_params(&requests, "thread/turns/list").is_empty());
    assert!(!recorded_params(&requests, "thread/items/list").is_empty());

    let previous_read_count = paginated_reads.len();
    let preview = crate::resume_picker::load_transcript_preview(
        &mut app_server,
        legacy_thread_id,
        Some(&app.config),
    )
    .await?;
    assert!(!preview.is_empty());
    let preview_reads = recorded_params(&requests, "thread/read");
    let preview_include_turns = preview_reads[previous_read_count..]
        .iter()
        .map(|params| params["includeTurns"].as_bool().unwrap_or(false))
        .collect::<Vec<_>>();
    assert_eq!(preview_include_turns, vec![false]);

    let previous_read_count = preview_reads.len();
    crate::thread_transcript::load_session_transcript(
        &mut app_server,
        legacy_thread_id,
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    )
    .await?;
    let legacy_reads = recorded_params(&requests, "thread/read");
    let legacy_include_turns = legacy_reads[previous_read_count..]
        .iter()
        .map(|params| params["includeTurns"].as_bool().unwrap_or(false))
        .collect::<Vec<_>>();
    assert_eq!(legacy_include_turns, vec![false, true]);

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn agents_overview_stop_uses_history_mode_for_turn_lookup() -> Result<()> {
    let (mut app, _codex_home) = make_history_test_app().await?;
    let paginated_thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "paginated background task",
    )?;
    let cases = [
        (paginated_thread_id, vec![false], 1),
        (
            create_history_rollout(
                &app.config,
                ThreadHistoryMode::Legacy,
                "legacy background task",
            )?,
            vec![false, true],
            0,
        ),
    ];
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::Current,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;

    for (thread_id, expected_include_turns, expected_turn_page_count) in cases {
        let previous_reads = recorded_params(&requests, "thread/read");
        let previous_turn_page_count = recorded_params(&requests, "thread/turns/list").len();

        app.stop_agents_overview_thread(&mut app_server, thread_id)
            .await;

        let reads = recorded_params(&requests, "thread/read");
        let include_turns = reads[previous_reads.len()..]
            .iter()
            .map(|params| params["includeTurns"].as_bool().unwrap_or(false))
            .collect::<Vec<_>>();
        assert_eq!(include_turns, expected_include_turns);
        assert_eq!(
            recorded_params(&requests, "thread/turns/list").len() - previous_turn_page_count,
            expected_turn_page_count
        );
    }

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn agents_overview_seeds_loaded_threads_when_recent_listing_is_unavailable() -> Result<()> {
    for (capabilities, expected_sort_keys) in [
        (
            HistoryCapabilities::LegacyOnly,
            vec!["recency_at", "recency_at", "updated_at", "updated_at"],
        ),
        (
            HistoryCapabilities::ThreadListFails,
            vec!["recency_at", "recency_at", "recency_at", "recency_at"],
        ),
    ] {
        let (mut app, _codex_home) = make_history_test_app().await?;
        let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
            &app.config,
            capabilities,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
        )
        .await?;
        let started = app_server.start_thread(&app.config).await?;
        app.app_server_target = AppServerTarget::LocalDaemon {
            endpoint: crate::RemoteAppServerEndpoint::UnixSocket {
                socket_path: test_path_buf("/tmp/unused.sock").abs(),
            },
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.app_event_tx = AppEventSender::new(tx);
        for attempt in 0..2 {
            if attempt == 0 {
                app.refresh_agents_overview_threads(&app_server);
            } else {
                app.open_agents_overview(&app_server);
            }
            let Some(AppEvent::AgentsOverviewThreadsLoaded { request_id, result }) =
                tokio::time::timeout(Duration::from_secs(10), rx.recv()).await?
            else {
                panic!("expected overview result")
            };
            app.apply_agents_overview_thread_refresh(&app_server, request_id, result);
            assert_eq!(
                app.agents_overview
                    .threads
                    .keys()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![started.session.thread_id]
            );
            assert_eq!(
                app.agents_overview.initialized,
                capabilities != HistoryCapabilities::ThreadListFails || attempt > 0
            );
            if attempt == 0 {
                app.handle_app_server_event(
                    &app_server,
                    AppServerEvent::ServerNotification(Box::new(
                        ServerNotification::ThreadStatusChanged(
                            codex_app_server_protocol::ThreadStatusChangedNotification {
                                thread_id: started.session.thread_id.to_string(),
                                status: codex_app_server_protocol::ThreadStatus::Idle,
                            },
                        ),
                    )),
                )
                .await;
                assert!(app.agents_overview.request_id.is_none());
            }
        }
        let list_requests = recorded_params(&requests, "thread/list");
        let mut sort_keys = list_requests
            .iter()
            .map(|params| params["sortKey"].as_str().unwrap())
            .collect::<Vec<_>>();
        sort_keys.sort_unstable();
        assert_eq!(sort_keys, expected_sort_keys);
        app_server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn agents_overview_stop_uses_full_history_after_legacy_negotiation() -> Result<()> {
    let (mut app, _codex_home) = make_history_test_app().await?;
    let thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "paginated background task",
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::LegacyOnly,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
    )
    .await?;
    app_server.start_thread(&app.config).await?;

    app.stop_agents_overview_thread(&mut app_server, thread_id)
        .await;

    let include_turns = recorded_params(&requests, "thread/read")
        .into_iter()
        .map(|params| params["includeTurns"].as_bool().unwrap_or(false))
        .collect::<Vec<_>>();
    assert_eq!(include_turns, vec![false, true]);
    assert!(recorded_params(&requests, "thread/turns/list").is_empty());

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn paginated_backtrack_uses_thread_revert_and_rehydrates_history() -> Result<()> {
    const BEFORE_TURN_ID: &str = "paginated-backtrack-turn";

    let (app, _codex_home) = make_history_test_app().await?;
    let thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "paginated prompt to edit",
    )?;
    let path = rollout_path(
        app.config.codex_home.as_path(),
        "2026-01-02T00-00-00",
        thread_id.to_string().as_str(),
    );
    let mut contents = std::fs::read_to_string(&path)?;
    for (ordinal, payload) in [
        serde_json::json!({
            "type": "task_started",
            "turn_id": BEFORE_TURN_ID,
            "model_context_window": null,
        }),
        serde_json::json!({
            "type": "item_completed",
            "thread_id": thread_id,
            "turn_id": BEFORE_TURN_ID,
            "item": {
                "type": "UserMessage",
                "id": "paginated-backtrack-user",
                "content": [{
                    "type": "text",
                    "text": "paginated prompt to edit",
                }],
            },
        }),
        serde_json::json!({
            "type": "task_complete",
            "turn_id": BEFORE_TURN_ID,
            "last_agent_message": null,
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let record = serde_json::json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "ordinal": ordinal + 3,
            "type": "event_msg",
            "payload": payload,
        });
        contents.push_str(&format!("{record}\n"));
    }
    std::fs::write(path, contents)?;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    app_server
        .resume_thread(
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let turn_page = app_server
        .thread_turns_page(thread_id, /*cursor*/ None)
        .await?;
    let [persisted_turn] = turn_page.data.as_slice() else {
        panic!("paginated fixture should materialize one turn");
    };
    assert_eq!(persisted_turn.id, BEFORE_TURN_ID);
    let before_turn_id = persisted_turn.id.clone();
    requests.lock().expect("request recorder lock").clear();

    let response = app_server
        .thread_revert_for_backtrack(&app.config, thread_id, before_turn_id.clone())
        .await?;

    assert!(response.thread.turns.is_empty());
    assert_eq!(
        recorded_params(&requests, "thread/revert"),
        vec![serde_json::json!({
            "threadId": thread_id,
            "beforeTurnId": before_turn_id,
        })]
    );
    assert!(recorded_params(&requests, "thread/rollback").is_empty());
    assert!(!recorded_params(&requests, "thread/turns/list").is_empty());
    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn cold_paginated_subagent_transcript_excludes_inherited_parent_history() -> Result<()> {
    let (app, codex_home) = make_history_test_app().await?;
    let parent_thread_id = create_history_rollout(
        &app.config,
        ThreadHistoryMode::Paginated,
        "parent-only paginated history",
    )?;
    let child_timestamp = "2026-01-02T00-00-01";
    let child_thread_id = ThreadId::from_string(
        &create_fake_parented_rollout_with_source(
            codex_home.path(),
            child_timestamp,
            "2026-01-02T00:00:01Z",
            "child-only paginated history",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(
                    AgentPath::try_from("/root/worker").map_err(color_eyre::eyre::Report::msg)?,
                ),
                agent_nickname: Some("worker".to_string()),
                agent_role: Some("worker".to_string()),
            }),
            parent_thread_id.into(),
            parent_thread_id,
        )
        .map_err(|err| color_eyre::eyre::eyre!("failed to create subagent rollout: {err}"))?,
    )?;
    let child_rollout_path = rollout_path(
        codex_home.path(),
        child_timestamp,
        &child_thread_id.to_string(),
    );
    let mut child_lines = std::fs::read_to_string(&child_rollout_path)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let mut meta = child_lines.remove(/*index*/ 0);
    meta["payload"]["history_mode"] = serde_json::json!("paginated");
    meta["payload"]["subagent_history_start_ordinal"] = serde_json::json!(3);
    meta["ordinal"] = serde_json::json!(0);
    for (index, line) in child_lines.iter_mut().enumerate() {
        line["ordinal"] = serde_json::json!(index + 3);
    }
    let rollout_record = |ordinal: usize, kind: &str, payload: serde_json::Value| {
        serde_json::json!({
            "timestamp": "2026-01-02T00:00:01Z",
            "ordinal": ordinal,
            "type": kind,
            "payload": payload,
        })
    };
    let inherited_response = rollout_record(
        /*ordinal*/ 1,
        "response_item",
        serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": "parent-only paginated history",
            }],
        }),
    );
    let inherited_event = rollout_record(
        /*ordinal*/ 2,
        "event_msg",
        serde_json::json!({
            "type": "user_message",
            "message": "parent-only paginated history",
            "kind": "plain",
        }),
    );
    let lines = std::iter::once(meta)
        .chain([inherited_response, inherited_event])
        .chain(child_lines)
        .chain([
            rollout_record(
                /*ordinal*/ 5,
                "event_msg",
                serde_json::json!({
                    "type": "task_started",
                    "turn_id": "child-visible-turn",
                    "model_context_window": null,
                }),
            ),
            rollout_record(
                /*ordinal*/ 6,
                "event_msg",
                serde_json::json!({
                    "type": "item_completed",
                    "thread_id": child_thread_id,
                    "turn_id": "child-visible-turn",
                    "item": {
                        "type": "UserMessage",
                        "id": "child-visible-user",
                        "content": [{
                            "type": "text",
                            "text": "child-only paginated history",
                        }],
                    },
                }),
            ),
        ])
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(child_rollout_path, format!("{lines}\n"))?;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;

    let resumed = app_server
        .resume_thread(
            app.config.clone(),
            child_thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let child_turn_page = app_server
        .thread_turns_page(child_thread_id, /*cursor*/ None)
        .await?;
    let child_item_page = app_server
        .thread_items_page(
            child_thread_id,
            /*turn_id*/ None,
            /*cursor*/ None,
            /*limit*/ 16,
        )
        .await?;
    let [child_turn] = child_turn_page.data.as_slice() else {
        panic!("paginated subagent should expose exactly one child turn");
    };
    let [child_entry] = child_item_page.data.as_slice() else {
        panic!("paginated subagent should expose exactly one child message");
    };
    let ThreadItem::UserMessage { content, .. } = &child_entry.item else {
        panic!("paginated subagent should expose its child user message");
    };
    assert_eq!(resumed.session.thread_id, child_thread_id);
    assert_eq!(child_entry.turn_id, child_turn.id);
    assert_eq!(
        content,
        &[UserInput::Text {
            text: "child-only paginated history".to_string(),
            text_elements: Vec::new(),
        }],
    );
    assert_eq!(
        resumed
            .turns
            .iter()
            .flat_map(|turn| turn.items.iter())
            .collect::<Vec<_>>(),
        vec![&child_entry.item],
    );

    let cells = crate::thread_transcript::load_session_transcript(
        &mut app_server,
        child_thread_id,
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    )
    .await?;
    let visible_history = cells
        .iter()
        .map(|cell| lines_to_single_string(&cell.display_lines(/*width*/ 80)))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible_history.contains("child-only paginated history"));
    assert!(!visible_history.contains("parent-only paginated history"));
    assert!(
        recorded_params(&requests, "thread/read")
            .iter()
            .all(|params| params["includeTurns"] != true)
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn changing_directory_preserves_project_trust_permissions_history_and_hooks() -> Result<()> {
    use codex_protocol::config_types::TrustLevel as T;
    use serde_json::json;
    use std::fs;

    let (mut app, mut events, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    app.harness_overrides.permission_profile = Some(PermissionProfile::workspace_write());
    let names = ["root", "trusted", "unknown", "untrusted", "p", "failure"];
    let [current, trusted, unknown, untrusted, mismatch, failed] =
        names.map(|name| codex_home.path().join(name));
    fs::create_dir_all(&current)?;
    for directory in [&trusted, &unknown, &untrusted, &mismatch, &failed] {
        fs::create_dir_all(directory.join(".codex"))?;
        fs::write(directory.join(".codex/config.toml"), "")?;
    }
    let contents = "developer_instructions = \"destination policy\"\nmodel_reasoning_effort = \"high\"\napproval_policy = \"on-request\"\n[tui]\ntheme = \"dracula\"\n[tui.keymap.global]\nopen_transcript = \"f12\"";
    fs::write(trusted.join(".codex/config.toml"), contents)?;
    let agents = trusted.join("AGENTS.md");
    fs::write(&agents, "Follow destination project instructions.")?;
    let hooks = r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"x"}]}]}}"#;
    fs::write(trusted.join(".codex/hooks.json"), hooks)?;
    let contents = "default_permissions = \"dev\"\n[permissions.dev.filesystem]\n\":root\" = \"write\"\n[tui.keymap.global]\nopen_transcript = \"ctrl-l\"";
    fs::write(mismatch.join(".codex/config.toml"), contents)?;
    let requirements = codex_home.path().join("requirements.toml");
    let rules = "allowed_approval_policies=[\"untrusted\"]\nallowed_sandbox_modes=[\"read-only\"]";
    fs::write(&requirements, rules)?;
    fs::create_dir_all(unknown.join(".git"))?;
    for dir in [&trusted, &untrusted, &mismatch, &failed] {
        let trust = [T::Trusted, T::Untrusted][usize::from(dir == &untrusted)];
        crate::legacy_core::config::set_project_trust_level(codex_home.path(), dir, trust)
            .map_err(|error| color_eyre::eyre::eyre!(error.to_string()))?;
    }
    app.config.cwd = current.clone().abs();
    app.chat_widget
        .handle_thread_session_quiet(test_thread_session(ThreadId::new(), current.clone()));
    let (no_list, nick, role, runtime, background) = (None, None, None, None, Some("background"));
    let (mut server, requests, proxy) =
        start_recording_app_server(&app.config, no_list, background).await?;
    let (rec, plain, req) = (recorded_params, crate::key_hint::plain, &requests);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let (source, message, name) = (None, None, Some("Previous project".to_string()));
    app.start_fresh_session_with_summary_hint(&mut tui, &mut server, source, message, name)
        .await;
    let original = app.chat_widget.thread_id().expect("original thread");
    let rollout = app.chat_widget.rollout_path().expect("original rollout");
    let json = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"saved history"}]}"#;
    let item = serde_json::from_str(json)?;
    server.thread_inject_items(original, vec![item]).await?;
    let tracked = server.start_thread(&app.config).await?;
    let (child, capacity) = (tracked.session.thread_id, THREAD_EVENT_CHANNEL_CAPACITY);
    let channel = ThreadEventChannel::new_with_session(capacity, tracked.session, tracked.turns);
    app.thread_event_channels.insert(child, channel);
    app.agent_navigation
        .upsert(child, nick, role, /*is_closed*/ false);
    let store = app.thread_event_channels[&child].store.clone();
    let config_path = codex_home.path().join("config.toml");
    let original_user_config = fs::read_to_string(&config_path).ok();
    let (local, url) = (app.environment_manager.clone(), Some("ws://[::1]".into()));
    let remote = Arc::new(EnvironmentManager::create_for_tests(url, runtime).await);
    let mut history = || {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell),
                _ => None,
            })
            .map(|cell| lines_to_single_string(&cell.display_lines(/*width*/ 200)))
            .collect::<Vec<_>>()
    };
    let change = |thread_id, path: &str| AppEvent::ChangeWorkingDirectory {
        thread_id,
        requested_cwd: path.into(),
    };
    for (path, kind, expected) in [
        ("missing", "local", "Cannot access directory"),
        ("../config.toml", "local", "Not a directory"),
        (r"C:\bad", "workspace", "not supported for remote"),
        ("~", "executor", "not supported for remote"),
        ("../trusted", "stale", "requires an idle primary session"),
        ("../trusted", "running", "another agent is running"),
        ("../trusted", "active", "another agent is running"),
        ("../trusted", "mcp", "inventory is still loading"),
        ("../trusted", "approval", "approval policy override"),
        ("../trusted", "profile", "permission profile override"),
        ("../trusted", "reviewer", "reviewer"),
        ("../p", "named", "different settings"),
        (
            "../trusted",
            "restored",
            "Permission profile cannot be preserved",
        ),
        ("../p", "keymap", "open_transcript"),
        ("../unknown", "local", "This directory is not trusted"),
        ("../trusted", "main", "background terminals"),
        ("../trusted", "child", "background terminals"),
    ] {
        app.config.approvals_reviewer = ApprovalsReviewer::User;
        if kind == "reviewer" {
            app.config.approvals_reviewer = ApprovalsReviewer::AutoReview;
            fs::write(&requirements, "allowed_approvals_reviewers = [\"user\"]")?;
        }
        app.agent_navigation.set_running(child, kind == "running");
        if kind == "active" {
            store.lock().await.set_active_turn_id("active".into());
        } else {
            store.lock().await.clear_active_turn_id();
        }
        app.loader_overrides.system_requirements_path =
            matches!(kind, "approval" | "profile" | "reviewer").then_some(requirements.clone());
        app.harness_overrides.permission_profile =
            (kind != "named").then_some(PermissionProfile::workspace_write());
        app.runtime_approval_policy_override = (kind == "approval").then_some(
            RuntimeApprovalPolicyOverride::Explicit(AskForApproval::OnRequest),
        );
        let mut profile = RuntimePermissionProfileOverride::from_config(&app.config);
        profile.active_permission_profile =
            (kind == "named").then(|| ActivePermissionProfile::new("dev"));
        if kind == "restored" {
            profile.permission_profile = PermissionProfile::workspace_write_with(
                &[failed.clone().abs()],
                codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
                /*exclude_tmpdir_env_var*/ false,
                /*exclude_slash_tmp*/ false,
            );
            profile.turn_override = RuntimePermissionProfileTurnOverride::Preserve;
        }
        app.runtime_permission_profile_override =
            matches!(kind, "profile" | "reviewer" | "named" | "restored").then_some(profile);
        app.app_server_target = crate::AppServerTarget::Embedded;
        if kind == "workspace" {
            let endpoint = crate::resolve_remote_addr("ws://127.0.0.1:8765")?;
            app.app_server_target = crate::AppServerTarget::Remote { endpoint };
        }
        app.environment_manager = [&local, &remote][usize::from(kind == "executor")].clone();
        requests.lock().expect("request recorder lock").clear();
        if kind == "mcp" {
            let loading = history_cell::new_mcp_inventory_loading;
            app.transcript_cells
                .push(Arc::new(loading(/*animations_enabled*/ false)));
        } else if kind == "child" {
            app.thread_event_channels.remove(&child);
        }
        let thread_id = [original, ThreadId::new()][usize::from(kind == "stale")];
        app.handle_event(&mut tui, &mut server, change(thread_id, path))
            .await?;
        assert_eq!(app.chat_widget.thread_id(), Some(original));
        assert_eq!(app.config.cwd, current.clone().abs());
        assert!(app.runtime_working_directory_override.is_none());
        let count = requests.lock().expect("request recorder lock").len();
        let checked = usize::from(kind == "main") + 2 * usize::from(kind == "child");
        assert_eq!(count, checked, "{kind}");
        let listed = recorded_params(&requests, "thread/backgroundTerminals/list");
        let mut ids = listed.iter().zip([original, child]);
        assert!(ids.all(|(p, id)| p["threadId"] == id.to_string()));
        assert_eq!(fs::read_to_string(&config_path).ok(), original_user_config);
        let output = history().join("");
        if kind == "mcp" {
            assert_snapshot!(output, @"■ MCP inventory is still loading.");
        } else if kind == "restored" {
            assert_snapshot!(output, @"■ Permission profile cannot be preserved by /cd.");
        }
        assert!(output.contains(expected), "{path}");
        app.clear_committed_mcp_inventory_loading();
    }
    let tracked = server.start_thread(&app.config).await?;
    let closed = tracked.session.thread_id;
    let channel = ThreadEventChannel::new_with_session(
        THREAD_EVENT_CHANNEL_CAPACITY,
        tracked.session,
        tracked.turns,
    );
    app.thread_event_channels.insert(closed, channel);
    app.agent_navigation.mark_closed(child);
    for has_stale_replay_turn in [false, true] {
        app.agent_navigation.upsert(
            closed, /*agent_nickname*/ None, /*agent_role*/ None,
            /*is_closed*/ false,
        );
        let channel = app.thread_event_channels.get_mut(&closed).expect("channel");
        channel.store.lock().await.set_turns(vec![test_turn(
            "stale-turn",
            TurnStatus::InProgress,
            Vec::new(),
        )]);
        if has_stale_replay_turn {
            channel.mark_replay_only();
            app.agent_navigation.mark_closed(closed);
        } else {
            app.enqueue_thread_notification(closed, thread_closed_notification(closed))
                .await?;
        }
        requests.lock().expect("request recorder lock").clear();
        app.change_working_directory(&mut tui, &mut server, failed.clone().abs())
            .await;
        assert_eq!(
            recorded_params(&requests, "thread/backgroundTerminals/list"),
            vec![json!({"threadId": original.to_string(), "cursor": null, "limit": 1})],
        );
        let output = history().join("");
        insta::allow_duplicates! {
            assert_snapshot!(output, @"■ Failed to change: thread/fork failed during TUI bootstrap: thread/fork failed: forced thread/name/set failure (code -32603)");
        }
    }
    app.agent_navigation.upsert(
        child, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.set_approvals_reviewer_in_app_and_widget(ApprovalsReviewer::AutoReview);
    app.runtime_permission_profile_override =
        Some(RuntimePermissionProfileOverride::from_config(&app.config));
    for (path, expected) in [(&failed, 0), (&trusted, 2)] {
        requests.lock().expect("request recorder lock").clear();
        app.change_working_directory(&mut tui, &mut server, path.clone().abs())
            .await;
        assert_eq!(app.chat_widget.thread_id(), Some(original));
        assert_eq!(app.config.cwd, current.clone().abs());
        assert_eq!(rec(&requests, "thread/unsubscribe").len(), expected);
        assert!(history().join("").contains("change"));
    }
    let removed = recorded_params(&requests, "thread/unsubscribe");
    let archived = recorded_params(&requests, "thread/archive");
    assert_eq!(removed[0]["threadId"], original.to_string());
    assert_eq!(archived[0]["threadId"], removed[1]["threadId"]);
    requests.lock().expect("request recorder lock").clear();
    app.handle_event(&mut tui, &mut server, change(original, "../trusted"))
        .await?;
    let forked = app.chat_widget.thread_id().expect("forked thread");
    assert_ne!(forked, original);
    let forked_rollout = app.chat_widget.rollout_path().expect("forked rollout");
    assert!(fs::read_to_string(&rollout)?.contains("saved history"));
    let copied = fs::read_to_string(&forked_rollout)?;
    let meta = codex_rollout::read_session_meta_line(&forked_rollout).await?;
    let base = meta.meta.history_base;
    assert!(copied.contains("saved history") || base.is_some_and(|h| h.thread_id == original));
    assert_eq!(app.config.cwd, trusted.clone().abs());
    let configured = app.primary_session_configured.as_ref().expect("session");
    let source = codex_utils_path_uri::PathUri::from_abs_path(&agents.abs());
    assert!(configured.instruction_source_paths.contains(&source));
    let (cwd, result) = (current.clone(), Err("stale skills".into()));
    let skills = AppEvent::SkillsListLoaded { cwd, result };
    let (cwd, plugins) = (current.clone(), Some(vec![]));
    let plugins = AppEvent::PluginMentionsLoaded { cwd, plugins };
    let diff = AppEvent::DiffResult(current.clone(), "stale diff".to_string());
    let branch = AppEvent::SyncThreadGitBranch {
        thread_id: original,
        branch: "stale".to_string(),
        cwd: current.clone(),
    };
    for event in [diff, skills, plugins, branch] {
        app.handle_event(&mut tui, &mut server, event).await?;
    }
    let path = trusted.to_str().expect("trusted path");
    let output = history().join("").replace(path, "<PROJECT>");
    let message = &output[output.rfind('•').expect("change")..];
    assert_snapshot!(message, @"• Working directory changed to: <PROJECT>");
    assert!(!output.contains("stale skills"));
    assert!(app.overlay.is_none());
    assert_eq!(app.keymap.app.open_transcript, vec![plain(KeyCode::F(12))]);
    let anchor = app.runtime_working_directory_override.as_deref();
    assert_eq!(anchor, Some(trusted.as_path()));
    let effort = app.chat_widget.current_reasoning_effort();
    assert_eq!(effort, Some(ReasoningEffortConfig::High));
    let approval = app.config.permissions.approval_policy.value();
    assert_eq!(approval, AskForApproval::OnRequest.to_core());
    let forks = recorded_params(&requests, "thread/fork");
    assert_eq!(forks.len(), 1);
    let params = &forks[0];
    assert_eq!(params["threadId"], serde_json::json!(original.to_string()));
    assert_eq!(params["cwd"], serde_json::json!(trusted));
    assert_eq!(params["approvalsReviewer"].as_str(), Some("auto_review"));
    assert_eq!(params["developerInstructions"], "destination policy");
    assert_eq!(params["deferGoalContinuation"], serde_json::json!(true));
    assert_eq!(&params["runtimeWorkspaceRoots"], &json!([trusted]));
    assert_eq!(rec(&requests, "hooks/list")[0]["cwds"], json!([trusted]));
    for suffix in "start resume settings/update archive".split(' ') {
        assert!(recorded_params(&requests, &format!("thread/{suffix}")).is_empty());
    }
    assert!(rec(req, "thread/metadata/update")[0]["threadId"] == params["threadId"]);
    let removed = recorded_params(&requests, "thread/unsubscribe");
    assert_eq!(removed.len(), 2);
    let found = |id: ThreadId| removed.iter().any(|p| p["threadId"] == id.to_string());
    assert!([original, child].into_iter().all(found));
    let retained = server.thread_read(original, /*include_turns*/ false);
    assert_eq!(retained.await?.cwd, current.abs().canonicalize()?);
    assert_eq!(app.chat_widget.config_ref().cwd, trusted.clone().abs());
    assert!(render_bottom_popup(&app.chat_widget, /*width*/ 80).contains("SessionStart"));
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.harness_overrides.bypass_hook_trust = Some(true);
    requests.lock().expect("request recorder lock").clear();
    app.change_working_directory(&mut tui, &mut server, trusted.abs())
        .await;
    assert!(app.config.bypass_hook_trust && !app.chat_widget.has_active_view());
    assert!(recorded_params(&requests, "hooks/list").is_empty());
    app.harness_overrides.bypass_hook_trust = None;
    requests.lock().expect("request recorder lock").clear();
    app.change_working_directory(&mut tui, &mut server, untrusted.clone().abs())
        .await;
    assert_eq!(app.config.active_project.trust_level, Some(T::Untrusted));
    let approval = app.config.permissions.approval_policy.value();
    assert_eq!(approval, AskForApproval::UnlessTrusted.to_core());
    assert_eq!(rec(req, "thread/fork")[0]["approvalPolicy"], "untrusted");
    let warning = "Project-local config, hooks, and exec policies are disabled";
    assert!(history().iter().any(|line| line.contains(warning)));
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn replay_only_user_turn_resumes_before_starting_exact_prompt() -> Result<()> {
    let (mut app, mut app_event_rx, mut op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let root_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2026-01-01T00-00-00",
            "2026-01-01T00:00:00Z",
            "Saved root message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create root rollout"),
    )?;
    let thread_id = ThreadId::from_string(
        &create_fake_parented_rollout_with_source(
            codex_home.path(),
            "2026-01-01T00-00-01",
            "2026-01-01T00:00:01Z",
            "Saved child message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: Some(AgentPath::try_from("/root/worker").expect("valid agent path")),
                agent_nickname: Some("worker".to_string()),
                agent_role: Some("worker".to_string()),
            }),
            root_thread_id.into(),
            root_thread_id,
        )
        .expect("create replay-only rollout"),
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let session = test_thread_session(thread_id, app.config.cwd.to_path_buf());
    let receiver = {
        let channel = app.ensure_thread_channel(thread_id);
        channel
            .store
            .lock()
            .await
            .set_session(session.clone(), Vec::new());
        channel.receiver.take()
    };
    app.active_thread_id = Some(thread_id);
    app.active_thread_rx = receiver;
    app.chat_widget.handle_thread_session(session);
    app.upsert_agent_picker_thread(
        thread_id,
        Some("worker".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ false,
    );
    app.mark_agent_picker_thread_closed(thread_id);
    assert_eq!(
        app.thread_event_channels
            .get(&thread_id)
            .map(ThreadEventChannel::attachment),
        Some(ThreadEventAttachment::ReplayOnly)
    );
    requests.lock().expect("request recorder lock").clear();
    while app_event_rx.try_recv().is_ok() {}

    app.chat_widget
        .restore_user_message_to_composer(crate::chatwidget::UserMessage::from("continue"));
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let op = next_user_turn_op(&mut op_rx);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    while let Ok(event) = app_event_rx.try_recv() {
        app.handle_event(&mut tui, &mut app_server, event).await?;
    }

    let control = app
        .handle_event(&mut tui, &mut app_server, AppEvent::CodexOp(op))
        .await?;

    assert!(matches!(control, AppRunControl::Continue));
    let recorded = requests.lock().expect("request recorder lock").clone();
    let lifecycle = recorded
        .iter()
        .filter(|request| matches!(request.method.as_str(), "thread/resume" | "turn/start"))
        .map(|request| request.method.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        vec!["thread/resume".to_string(), "turn/start".to_string()]
    );
    assert!(recorded.iter().any(|request| {
        request.method == "turn/start"
            && request
                .params
                .as_ref()
                .and_then(|params| params.get("input"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("text").and_then(serde_json::Value::as_str) == Some("continue")
                    })
                })
    }));
    assert_eq!(
        app.thread_event_channels
            .get(&thread_id)
            .map(ThreadEventChannel::attachment),
        Some(ThreadEventAttachment::Live)
    );
    assert!(
        !app.agent_navigation
            .get(&thread_id)
            .expect("resumed agent navigation entry")
            .is_closed
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn closed_child_selection_restores_model_and_effort_from_rollout() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let root_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2026-01-01T00-00-00",
            "2026-01-01T00:00:00Z",
            "Saved root message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create root rollout"),
    )?;
    let child_timestamp = "2026-01-01T00-00-01";
    let child_thread_id = ThreadId::from_string(
        &create_fake_parented_rollout_with_source(
            codex_home.path(),
            child_timestamp,
            "2026-01-01T00:00:01Z",
            "Saved child message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: Some(AgentPath::try_from("/root/worker").expect("valid agent path")),
                agent_nickname: Some("worker".to_string()),
                agent_role: Some("worker".to_string()),
            }),
            root_thread_id.into(),
            root_thread_id,
        )
        .expect("create child rollout"),
    )?;
    let child_rollout_path = rollout_path(
        codex_home.path(),
        child_timestamp,
        &child_thread_id.to_string(),
    );
    let mut child_rollout = std::fs::read_to_string(&child_rollout_path)?;
    child_rollout.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "timestamp": "2026-01-01T00:00:02Z",
            "type": "turn_context",
            "payload": {
                "cwd": app.config.cwd.as_path(),
                "approval_policy": "never",
                "sandbox_policy": {"type": "danger-full-access"},
                "model": "gpt-5.6-sol",
                "effort": "low",
                "summary": "auto",
            },
        })
    ));
    std::fs::write(&child_rollout_path, child_rollout)?;

    let (mut app_server, _requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let root = app_server
        .resume_thread(
            app.config.clone(),
            root_thread_id,
            app.resume_model_settings(),
        )
        .await?;
    app.enqueue_primary_thread_session(root.session, root.turns)
        .await?;
    while app_event_rx.try_recv().is_ok() {}

    app.thread_event_channels.insert(
        child_thread_id,
        ThreadEventChannel::new(THREAD_EVENT_CHANNEL_CAPACITY),
    );
    app.upsert_agent_picker_thread(
        child_thread_id,
        Some("worker".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ true,
    );
    app.agent_navigation
        .set_parent_thread_id(child_thread_id, Some(root_thread_id));
    let mut tui = crate::tui::test_support::make_test_tui()?;

    app.select_agent_thread(&mut tui, &mut app_server, child_thread_id)
        .await?;

    assert_eq!(app.chat_widget.current_model(), "gpt-5.6-sol");
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::Low)
    );
    let session_model_line = std::iter::from_fn(|| app_event_rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(cell),
            _ => None,
        })
        .flat_map(|cell| cell.display_lines(/*width*/ 100))
        .map(|line| lines_to_single_string(&[line]))
        .find(|line| line.contains("model:"))
        .expect("restored session header model line");
    let session_model_line = session_model_line
        .trim_matches(|character: char| character == '│' || character.is_whitespace())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let footer = render_bottom_popup(&app.chat_widget, /*width*/ 120);
    let footer_model_line = footer
        .lines()
        .find(|line| line.contains("gpt-5.6-sol"))
        .expect("restored footer model line");
    let footer_model = footer_model_line[footer_model_line
        .find("gpt-5.6-sol")
        .expect("footer model position")..]
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    insta::assert_snapshot!(
        format!("session header: {session_model_line}\nfooter: {footer_model}"),
        @r"
    session header: model: gpt-5.6-sol low /model to change
    footer: gpt-5.6-sol low
    "
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[test]
fn fresh_session_applies_requested_name() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-named-fresh-session".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut app = make_test_app().await;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
                let (mut app_server, requests, proxy) = start_recording_app_server(
                    &app.config,
                    /*blocked_thread_list*/ None,
                    /*failed_thread_name*/ None,
                )
                .await?;
                let mut tui = crate::tui::test_support::make_test_tui()?;

                app.start_fresh_session_with_summary_hint(
                    &mut tui,
                    &mut app_server,
                    /*session_start_source*/ None,
                    /*initial_user_message*/ None,
                    /*new_thread_name*/ Some("Add User".to_string()),
                )
                .await;

                let thread_id = app
                    .chat_widget
                    .thread_id()
                    .expect("fresh session should have a thread id");
                assert_eq!(app.chat_widget.thread_name(), Some("Add User".to_string()));
                assert!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .any(|request| request.method == "thread/name/set"),
                    "fresh session should be named through the app server"
                );
                let thread = app_server
                    .thread_read(thread_id, /*include_turns*/ false)
                    .await?;
                assert_eq!(thread.name.as_deref(), Some("Add User"));

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("named fresh session test thread")
}

#[test]
fn session_lifecycle_avoids_redundant_subagent_metadata_reads() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-session-lifecycle-requests".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite =
                    codex_state::SqliteConfig::new_for_testing(codex_home.path().abs());
                let root_timestamp = "2026-01-01T00-00-00";
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        root_timestamp,
                        "2026-01-01T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-01T00-00-01",
                        "2026-01-01T00:00:01Z",
                        "Saved child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/worker").expect("valid agent path"),
                            ),
                            agent_nickname: Some("worker".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create child rollout"),
                )?;
                let root_rollout_path = rollout_path(
                    codex_home.path(),
                    root_timestamp,
                    &root_thread_id.to_string(),
                );
                let (started_tx, started_rx) = oneshot::channel();
                let (release_tx, release_rx) = oneshot::channel();
                let (mut app_server, requests, proxy) = start_recording_app_server(
                    &app.config,
                    Some((root_thread_id, started_tx, release_rx)),
                    Some("Failed Fork"),
                )
                .await?;
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;
                app_server
                    .resume_thread(
                        app.config.clone(),
                        child_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                let mut tui = crate::tui::test_support::make_test_tui()?;
                take_backfill_counts(&requests);

                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::ForkCurrentSession {
                        name: Some("Add User Fork".to_string()),
                    },
                ))
                .await?;

                assert!(matches!(control, AppRunControl::Continue));
                assert_ne!(app.chat_widget.thread_id(), Some(root_thread_id));
                let named_fork_id = app
                    .chat_widget
                    .thread_id()
                    .expect("named fork should have a thread id");
                assert_eq!(
                    app.chat_widget.thread_name(),
                    Some("Add User Fork".to_string())
                );
                // Forking may read the source metadata once when the response includes its parent
                // id. It must not scan or backfill loaded threads for the newly created fork.
                assert!(matches!(take_backfill_counts(&requests), (0, 0) | (0, 1)));
                let named_fork = app_server
                    .thread_read(named_fork_id, /*include_turns*/ false)
                    .await?;
                assert_eq!(named_fork.name.as_deref(), Some("Add User Fork"));
                take_backfill_counts(&requests);

                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::ForkCurrentSession {
                        name: Some("Failed Fork".to_string()),
                    },
                ))
                .await?;

                assert!(matches!(control, AppRunControl::Continue));
                assert_ne!(app.chat_widget.thread_id(), Some(named_fork_id));
                let name_error = std::iter::from_fn(|| app_event_rx.try_recv().ok())
                    .find_map(|event| match event {
                        AppEvent::InsertHistoryCell(cell) => {
                            let rendered =
                                lines_to_single_string(&cell.display_lines(/*width*/ 80));
                            rendered
                                .contains("Failed to name the forked session")
                                .then_some(rendered)
                        }
                        _ => None,
                    })
                    .expect("fork naming error history cell");
                insta::assert_snapshot!(
                    name_error,
                    @"■ Failed to name the forked session: thread/name/set failed in TUI"
                );
                assert!(matches!(take_backfill_counts(&requests), (0, 0) | (0, 1)));

                app.start_fresh_session_with_summary_hint(
                    &mut tui,
                    &mut app_server,
                    /*session_start_source*/ None,
                    /*initial_user_message*/ None,
                    /*new_thread_name*/ None,
                )
                .await;

                assert_ne!(app.chat_widget.thread_id(), Some(root_thread_id));
                assert_eq!(take_backfill_counts(&requests), (0, 0));

                let loaded_threads = app_server
                    .thread_loaded_list(ThreadLoadedListParams {
                        cursor: None,
                        limit: None,
                    })
                    .await?
                    .data;
                let expected_reads = loaded_threads
                    .iter()
                    .filter(|thread_id| *thread_id != &root_thread_id.to_string())
                    .count();
                assert!(loaded_threads.contains(&child_thread_id.to_string()));
                take_backfill_counts(&requests);
                app.harness_overrides.cwd = Some(app.config.cwd.to_path_buf());

                let control = app
                    .resume_target_session(
                        &mut tui,
                        &mut app_server,
                        crate::resume_picker::SessionTarget {
                            path: Some(root_rollout_path),
                            source_rollout_path: None,
                            thread_id: root_thread_id,
                            history_mode: None,
                        },
                    )
                    .await?;

                assert!(matches!(control, AppRunControl::Continue));
                assert_eq!(app.chat_widget.thread_id(), Some(root_thread_id));
                assert_eq!(take_backfill_counts(&requests), (1, expected_reads));
                assert_eq!(
                    app.agent_navigation.get(&child_thread_id),
                    Some(&AgentPickerThreadEntry {
                        agent_nickname: Some("worker".to_string()),
                        agent_role: Some("worker".to_string()),
                        agent_path: Some("/root/worker".to_string()),
                        is_running: false,
                        is_closed: false,
                    })
                );

                let child_store = Arc::clone(
                    &app.thread_event_channels
                        .entry(child_thread_id)
                        .or_insert_with(|| ThreadEventChannel::new(/*capacity*/ 1))
                        .store,
                );
                let child_store_guard = child_store.lock().await;
                futures::FutureExt::now_or_never(app.open_agent_picker(&mut app_server))
                    .expect("opening the agent picker waited for the app server");
                drop(child_store_guard);
                insta::assert_snapshot!(
                    render_bottom_popup(&app.chat_widget, /*width*/ 80)
                        .replace(&root_thread_id.to_string(), "[root]")
                        .replace(&child_thread_id.to_string(), "[child]"),
                    @r###"
                      Agents
                      Select an agent to watch. ⌥ + ← previous, ⌥ + → next.

                      Filter by ref, name, role, path, or UUID
                    › 1 • Main [default] (current)  [root] ·
                                                    completed
                      2 ↳ • worker [worker]         [child] · idle

                      Main [default]
                      completed · ref 1

                      UUID
                      [root]

                      Nickname
                      Main

                      Model: gpt-5.6-sol
                      Task: Saved user message

                      Response: none
                      Queued: 0
                      Children: 1

                      Enter opens this thread

                      Tab opens controls for the selected agent.
                      Press enter to confirm or esc to go back
                    "###
                );
                assert_eq!(take_backfill_counts(&requests), (0, 0));
                tokio::time::timeout(Duration::from_secs(5), started_rx).await??;
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                futures::FutureExt::now_or_never(app.open_agent_picker(&mut app_server))
                    .expect("reopening the agent picker waited for the app server");
                assert_eq!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .filter(|request| request.method == "thread/list")
                        .count(),
                    1
                );
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                app.chat_widget.handle_server_request(
                    exec_approval_request(
                        root_thread_id,
                        "turn",
                        "item",
                        /*approval_id*/ None,
                    ),
                    /*replay_kind*/ None,
                    std::time::Instant::now(),
                );
                app.agent_navigation.mark_stopped(child_thread_id);
                release_tx.send(()).expect("release blocked thread list");
                let discovered_thread_id = ThreadId::new();
                let mut completion = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let event = app_event_rx.recv().await.expect("app event channel");
                        if matches!(event, AppEvent::AgentPickerThreadsLoaded { .. }) {
                            break event;
                        }
                    }
                })
                .await?;
                if let AppEvent::AgentPickerThreadsLoaded {
                    result: Ok(refresh),
                    ..
                } = &mut completion
                {
                    let threads = &mut refresh.threads;
                    let child = threads
                        .iter_mut()
                        .find(|thread| thread.id == child_thread_id.to_string())
                        .expect("root-scoped response includes the cached child");
                    let mut discovered = child.clone();
                    discovered.id = discovered_thread_id.to_string();
                    child.status = ThreadStatus::Active {
                        active_flags: Vec::new(),
                    };
                    threads.push(discovered);
                }
                app.handle_event(&mut tui, &mut app_server, completion)
                    .await?;
                assert_eq!(
                    app.agent_navigation
                        .ordered_threads()
                        .last()
                        .map(|(thread_id, _)| *thread_id),
                    Some(discovered_thread_id)
                );
                assert_eq!(
                    app.chat_widget.selected_index_for_present_view(
                        super::super::agent_picker::AGENT_PICKER_VIEW_ID
                    ),
                    Some(1)
                );
                assert!(render_bottom_popup(&app.chat_widget, /*width*/ 80).contains("echo hello"));
                assert!(
                    app.agent_navigation
                        .get(&child_thread_id)
                        .is_some_and(|entry| !entry.is_running)
                );
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("session lifecycle request test thread")
}
