use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use app_test_support::to_response;
use app_test_support::write_models_cache;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RawResponseItemCompletedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadInjectItemsParams;
use codex_app_server_protocol::ThreadInjectItemsResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemsListParams;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;
use codex_rollout::append_rollout_item_to_path;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const CHILD_PROMPT: &str = "Reply with child done.";
const PARENT_PROMPT: &str = "Spawn a child.";
const SPAWN_CALL_ID: &str = "spawn-call-transcript";
const PLAINTEXT_TRANSCRIPT_TEXT: &str = "Agent message from `/root`:\n\nReply with child done.";
const ENCRYPTED_TRANSCRIPT_TEXT: &str = "Agent message from `/root`:\n\nInput message encrypted";
const ENCRYPTED_PAYLOAD: &str = "opaque-child-task";
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const NO_DUPLICATE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
enum MessageDeliveryCase {
    Plaintext,
    Encrypted,
    EncryptedWithAudit,
}

impl MessageDeliveryCase {
    fn config_value(self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Encrypted => "encrypted",
            Self::EncryptedWithAudit => "encrypted_with_audit",
        }
    }

    fn child_request_marker(self) -> &'static str {
        match self {
            Self::Plaintext => CHILD_PROMPT,
            Self::Encrypted | Self::EncryptedWithAudit => ENCRYPTED_PAYLOAD,
        }
    }

    fn transcript_text(self) -> &'static str {
        match self {
            Self::Plaintext => PLAINTEXT_TRANSCRIPT_TEXT,
            Self::Encrypted | Self::EncryptedWithAudit => ENCRYPTED_TRANSCRIPT_TEXT,
        }
    }
}

#[tokio::test]
async fn inter_agent_input_is_typed_once_for_each_delivery_mode() -> Result<()> {
    skip_if_no_network!(Ok(()));

    for message_delivery in [
        MessageDeliveryCase::Plaintext,
        MessageDeliveryCase::Encrypted,
        MessageDeliveryCase::EncryptedWithAudit,
    ] {
        for experimental_raw_events in [false, true] {
            run_inter_agent_input_case(message_delivery, experimental_raw_events).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn injected_agent_message_gets_a_stable_id_without_starting_model_work() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_multi_agent_config(codex_home.path(), &server.uri(), "plaintext")?;
    write_models_cache(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;

    let thread_request = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            experimental_raw_events: true,
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let thread_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(thread_response)?;
    let transcript_text = "Agent message from `/root`:\n\nInjected task.";
    let injected_item = ResponseItem::AgentMessage {
        id: Some(ResponseItemId::with_suffix("msg", "caller")),
        author: "/root".to_string(),
        recipient: "/root/worker".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "Injected task.".to_string(),
        }],
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("caller-turn".to_string()),
        }),
    };

    let inject_request = app_server
        .send_thread_inject_items_request(ThreadInjectItemsParams {
            thread_id: thread.id.clone(),
            items: vec![serde_json::to_value(injected_item)?],
        })
        .await?;
    let inject_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(inject_request)),
    )
    .await??;
    let _: ThreadInjectItemsResponse = to_response(inject_response)?;

    let started = timeout(
        READ_TIMEOUT,
        read_inter_agent_item_started(&mut app_server, transcript_text),
    )
    .await??;
    let item_id = started.item.id().to_string();
    assert!(item_id.starts_with("amsg_"));
    assert_ne!(started.turn_id, "caller-turn");
    assert_eq!(
        uuid::Uuid::parse_str(&started.turn_id)?.get_version(),
        Some(uuid::Version::SortRand)
    );
    let typed = timeout(
        READ_TIMEOUT,
        read_inter_agent_item_by_id(&mut app_server, &item_id),
    )
    .await??;
    assert_eq!(typed.thread_id, started.thread_id);
    assert_eq!(typed.turn_id, started.turn_id);
    assert_eq!(typed.item, started.item);
    let raw = timeout(
        READ_TIMEOUT,
        read_raw_inter_agent_item(&mut app_server, &item_id),
    )
    .await??;
    assert_eq!(raw.turn_id, typed.turn_id);
    assert_eq!(
        raw.item.id().map(ToString::to_string),
        Some(item_id.clone())
    );

    let items_request = app_server
        .send_thread_items_list_request(ThreadItemsListParams {
            thread_id: thread.id.clone(),
            turn_id: Some(typed.turn_id.clone()),
            cursor: None,
            limit: None,
            sort_direction: Some(SortDirection::Asc),
        })
        .await?;
    let items_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(items_request)),
    )
    .await??;
    let ThreadItemsListResponse { data, .. } = to_response(items_response)?;
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].turn_id, typed.turn_id);
    assert_eq!(data[0].item, typed.item);

    let turns_request = app_server
        .send_thread_turns_list_request(ThreadTurnsListParams {
            thread_id: thread.id,
            cursor: None,
            limit: None,
            sort_direction: Some(SortDirection::Asc),
            items_view: Some(TurnItemsView::NotLoaded),
        })
        .await?;
    let turns_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(turns_request)),
    )
    .await??;
    let ThreadTurnsListResponse { data, .. } = to_response(turns_response)?;
    let injected_turn = data
        .iter()
        .find(|turn| turn.id == typed.turn_id)
        .expect("injected agent message turn");
    assert_eq!(injected_turn.status, TurnStatus::Completed);
    assert_eq!(injected_turn.items_view, TurnItemsView::NotLoaded);
    assert!(injected_turn.items.is_empty());

    Ok(())
}

#[tokio::test]
async fn resume_rebuilds_legacy_inter_agent_communication_variants() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_multi_agent_config(codex_home.path(), &server.uri(), "plaintext")?;
    write_models_cache(codex_home.path())?;
    let filename_timestamp = "2026-07-21T12-00-00";
    let thread_id = create_fake_rollout(
        codex_home.path(),
        filename_timestamp,
        "2026-07-21T12:00:00Z",
        "Saved user message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let rollout_path = rollout_path(codex_home.path(), filename_timestamp, &thread_id);
    let mut plaintext = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("worker path"),
        Vec::new(),
        "Inspect the repository.".to_string(),
        /*trigger_turn*/ true,
    );
    plaintext.set_turn_id_if_missing("legacy-plaintext");
    let mut encrypted = InterAgentCommunication::new_encrypted(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "opaque-result".to_string(),
        /*trigger_turn*/ false,
    );
    encrypted.set_turn_id_if_missing("legacy-encrypted");
    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::InterAgentCommunication(plaintext),
    )
    .await?;
    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::InterAgentCommunication(encrypted),
    )
    .await?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;
    let resume_request = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
            ..Default::default()
        })
        .await?;
    let resume_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(resume_request)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response(resume_response)?;
    let transcript_items = thread
        .turns
        .iter()
        .flat_map(|turn| {
            turn.items.iter().filter_map(|item| match item {
                ThreadItem::AgentMessage { id, text, .. } => {
                    Some((turn.id.as_str(), id.as_str(), text.as_str()))
                }
                _ => None,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transcript_items,
        vec![
            (
                "legacy-plaintext",
                "item-2",
                "Agent message from `/root`:\n\nInspect the repository.",
            ),
            (
                "legacy-encrypted",
                "item-3",
                "Agent message from `/root/worker`:\n\nInput message encrypted",
            ),
        ]
    );

    let transcript_turn_statuses = thread
        .turns
        .iter()
        .filter_map(|turn| match turn.id.as_str() {
            "legacy-plaintext" | "legacy-encrypted" => Some((turn.id.clone(), turn.status.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transcript_turn_statuses,
        vec![
            ("legacy-plaintext".to_string(), TurnStatus::Completed),
            ("legacy-encrypted".to_string(), TurnStatus::Completed),
        ]
    );

    Ok(())
}

async fn run_inter_agent_input_case(
    message_delivery: MessageDeliveryCase,
    experimental_raw_events: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let spawn_args = match message_delivery {
        MessageDeliveryCase::Plaintext => json!({
            "message": CHILD_PROMPT,
            "task_name": "worker",
            "fork_turns": "none",
        }),
        MessageDeliveryCase::Encrypted => json!({
            "message": ENCRYPTED_PAYLOAD,
            "task_name": "worker",
            "fork_turns": "none",
        }),
        MessageDeliveryCase::EncryptedWithAudit => json!({
            "message": ENCRYPTED_PAYLOAD,
            "task_message": CHILD_PROMPT,
            "task_name": "worker",
            "fork_turns": "none",
        }),
    };
    let spawn_args = serde_json::to_string(&spawn_args)?;
    let _parent_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, PARENT_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("resp-parent-1"),
            responses::ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                "collaboration",
                "spawn_agent",
                &spawn_args,
            ),
            responses::ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let _child_turn = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, message_delivery.child_request_marker())
                && !body_contains(request, SPAWN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-child-1"),
            responses::ev_assistant_message("msg-child-1", "child done"),
            responses::ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let _parent_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SPAWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-parent-2"),
            responses::ev_assistant_message("msg-parent-2", "parent done"),
            responses::ev_completed("resp-parent-2"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    write_multi_agent_config(
        codex_home.path(),
        &server.uri(),
        message_delivery.config_value(),
    )?;
    write_models_cache(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;

    let thread_request = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            experimental_raw_events,
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let turn_request = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![UserInput::Text {
                text: PARENT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(turn_request)),
    )
    .await??;
    let _: TurnStartResponse = to_response(turn_response)?;

    let started = timeout(
        READ_TIMEOUT,
        read_inter_agent_item_started(&mut app_server, message_delivery.transcript_text()),
    )
    .await??;
    let ThreadItem::AgentMessage { id, text, .. } = &started.item else {
        unreachable!("notification predicate requires an agent message");
    };
    assert!(id.starts_with("amsg_"));
    assert_eq!(text, message_delivery.transcript_text());
    assert!(!text.contains(ENCRYPTED_PAYLOAD));
    let typed = timeout(
        READ_TIMEOUT,
        read_inter_agent_item_by_id(&mut app_server, id),
    )
    .await??;
    assert_eq!(typed.thread_id, started.thread_id);
    assert_eq!(typed.turn_id, started.turn_id);
    assert_eq!(typed.item, started.item);

    if experimental_raw_events {
        let raw = timeout(READ_TIMEOUT, read_raw_inter_agent_item(&mut app_server, id)).await??;
        assert_eq!(raw.thread_id, typed.thread_id);
        assert_eq!(raw.turn_id, typed.turn_id);
        assert_eq!(raw.item.id().map(ToString::to_string), Some(id.to_string()));
        let ResponseItem::AgentMessage {
            author, content, ..
        } = raw.item
        else {
            unreachable!("notification predicate requires an agent message");
        };
        assert_eq!(author, "/root");
        match message_delivery {
            MessageDeliveryCase::Plaintext => {
                assert!(content.iter().all(|content| !matches!(
                    content,
                    codex_protocol::models::AgentMessageInputContent::EncryptedContent { .. }
                )));
            }
            MessageDeliveryCase::Encrypted | MessageDeliveryCase::EncryptedWithAudit => {
                assert!(content.iter().any(|content| matches!(
                    content,
                    codex_protocol::models::AgentMessageInputContent::EncryptedContent {
                        encrypted_content
                    } if encrypted_content == ENCRYPTED_PAYLOAD
                )));
            }
        }
    }

    let completed = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_matching_notification(
            "child turn/completed",
            |notification| {
                turn_completed(notification)
                    .is_some_and(|completed| completed.thread_id == typed.thread_id)
            },
        ),
    )
    .await??;
    let completed = turn_completed(&completed).expect("matching turn completion");
    assert_eq!(completed.turn.id, typed.turn_id);

    let items_request = app_server
        .send_thread_items_list_request(ThreadItemsListParams {
            thread_id: typed.thread_id.clone(),
            turn_id: Some(typed.turn_id.clone()),
            cursor: None,
            limit: None,
            sort_direction: None,
        })
        .await?;
    let items_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(items_request)),
    )
    .await??;
    let ThreadItemsListResponse { data, .. } = to_response(items_response)?;
    assert!(data.iter().any(|entry| {
        matches!(
            &entry.item,
            ThreadItem::AgentMessage {
                id: stored_id,
                text,
                ..
            } if stored_id == id && text == message_delivery.transcript_text()
        )
    }));

    assert!(
        timeout(
            NO_DUPLICATE_TIMEOUT,
            read_inter_agent_item_started_by_id(&mut app_server, id)
        )
        .await
        .is_err(),
        "inter-agent input should emit exactly one typed start"
    );
    assert!(
        timeout(
            NO_DUPLICATE_TIMEOUT,
            read_inter_agent_item_by_id(&mut app_server, id)
        )
        .await
        .is_err(),
        "inter-agent input should emit exactly one typed completion"
    );
    assert!(
        timeout(
            NO_DUPLICATE_TIMEOUT,
            read_raw_inter_agent_item(&mut app_server, id)
        )
        .await
        .is_err(),
        "raw inter-agent input notification count should match opt-in"
    );

    Ok(())
}

async fn read_inter_agent_item_started(
    app_server: &mut TestAppServer,
    transcript_text: &str,
) -> Result<ItemStartedNotification> {
    let notification = app_server
        .read_stream_until_matching_notification("inter-agent item/started", |notification| {
            item_started(notification).is_some_and(|started| {
                matches!(
                    &started.item,
                    ThreadItem::AgentMessage { text, .. } if text == transcript_text
                )
            })
        })
        .await?;
    Ok(item_started(&notification).expect("matching item start"))
}

async fn read_inter_agent_item_started_by_id(
    app_server: &mut TestAppServer,
    item_id: &str,
) -> Result<ItemStartedNotification> {
    let notification = app_server
        .read_stream_until_matching_notification("inter-agent item/started by id", |notification| {
            item_started(notification).is_some_and(|started| {
                matches!(
                    &started.item,
                    ThreadItem::AgentMessage { id, .. } if id == item_id
                )
            })
        })
        .await?;
    Ok(item_started(&notification).expect("matching item start"))
}

async fn read_inter_agent_item_by_id(
    app_server: &mut TestAppServer,
    item_id: &str,
) -> Result<ItemCompletedNotification> {
    let notification = app_server
        .read_stream_until_matching_notification(
            "inter-agent item/completed by id",
            |notification| {
                item_completed(notification).is_some_and(|completed| {
                    matches!(
                        &completed.item,
                        ThreadItem::AgentMessage { id, .. } if id == item_id
                    )
                })
            },
        )
        .await?;
    Ok(item_completed(&notification).expect("matching item completion"))
}

async fn read_raw_inter_agent_item(
    app_server: &mut TestAppServer,
    item_id: &str,
) -> Result<RawResponseItemCompletedNotification> {
    let notification = app_server
        .read_stream_until_matching_notification(
            "inter-agent rawResponseItem/completed",
            |notification| {
                raw_item_completed(notification).is_some_and(|completed| {
                    completed.item.id().is_some_and(|id| id.as_str() == item_id)
                })
            },
        )
        .await?;
    Ok(raw_item_completed(&notification).expect("matching raw item completion"))
}

fn item_completed(notification: &JSONRPCNotification) -> Option<ItemCompletedNotification> {
    if notification.method != "item/completed" {
        return None;
    }
    serde_json::from_value(notification.params.clone()?).ok()
}

fn item_started(notification: &JSONRPCNotification) -> Option<ItemStartedNotification> {
    if notification.method != "item/started" {
        return None;
    }
    serde_json::from_value(notification.params.clone()?).ok()
}

fn raw_item_completed(
    notification: &JSONRPCNotification,
) -> Option<RawResponseItemCompletedNotification> {
    if notification.method != "rawResponseItem/completed" {
        return None;
    }
    let completed: RawResponseItemCompletedNotification =
        serde_json::from_value(notification.params.clone()?).ok()?;
    matches!(&completed.item, ResponseItem::AgentMessage { .. }).then_some(completed)
}

fn turn_completed(notification: &JSONRPCNotification) -> Option<TurnCompletedNotification> {
    if notification.method != "turn/completed" {
        return None;
    }
    serde_json::from_value(notification.params.clone()?).ok()
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8(request.body.clone())
        .ok()
        .is_some_and(|body| body.contains(text))
}

fn write_multi_agent_config(
    codex_home: &Path,
    server_uri: &str,
    message_delivery: &str,
) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "mock_provider"

[features.multi_agent_v2]
enabled = true
message_delivery = "{message_delivery}"
tool_namespace = "collaboration"
non_code_mode_only = false

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
