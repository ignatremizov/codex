//! Public user-mailbox acceptance: authored input is durable before explicit consumption.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadMailboxAddParams;
use codex_app_server_protocol::ThreadMailboxAddResponse;
use codex_app_server_protocol::ThreadMailboxMessageState;
use codex_app_server_protocol::ThreadSectionMoveParams;
use codex_app_server_protocol::ThreadSectionMoveResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::ThreadId;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;
use codex_protocol::user_input::UserInput as CoreUserInput;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSender;
use codex_thread_store::RejectMailboxInputParams;
use codex_thread_store::ThreadStore;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::MockServer;

const READ_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 15);

#[tokio::test]
async fn first_mailbox_input_preserves_typed_user_authorship_without_payload_turn() -> Result<()> {
    let (mut app, _home, server, store) = mailbox_app(MultiAgentVersion::V1).await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    let input = vec![
        UserInput::from(CoreUserInput::Text {
            text: "note [image]".to_string(),
            text_elements: vec![TextElement::new(
                ByteRange { start: 5, end: 12 },
                Some("mail attachment".to_string()),
            )],
        }),
        UserInput::Image {
            url: "data:image/png;base64,mailbox-original-image".to_string(),
            detail: None,
        },
        UserInput::Mention {
            name: "mailbox-original-mention".to_string(),
            path: "app://mailbox-user-fixture".to_string(),
        },
    ];
    let params = ThreadMailboxAddParams {
        thread_id: thread.id.clone(),
        input: input.clone(),
        client_user_message_id: "first-mail".to_string(),
    };
    let accepted = add_mail(&mut app, params.clone()).await?;
    assert_eq!(
        accepted,
        ThreadMailboxAddResponse {
            message_id: accepted.message_id.clone(),
            state: ThreadMailboxMessageState::Pending,
            rejection_reason: None,
        },
    );
    assert_eq!(add_mail(&mut app, params.clone()).await?, accepted);
    let stored = store
        .lookup_mailbox_input(
            ThreadId::from_string(&thread.id)?,
            &serde_json::to_string(&("user-mailbox", "first-mail"))?,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("stored user acceptance"))?;
    assert_eq!(stored.id, accepted.message_id);
    assert_eq!(stored.sender, MailboxSender::User);
    assert_eq!(
        stored.payload,
        MailboxPayload::User {
            input: input.into_iter().map(UserInput::into_core).collect(),
            client_id: Some("first-mail".to_string()),
        },
    );
    // The real app-server may run inventory after acceptance. It must not sample the
    // accepted payload, nor issue a model request merely to resolve first-turn eligibility.
    for request in server.received_requests().await.unwrap_or_default() {
        if !request.url.path().ends_with("/responses") {
            continue;
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body)?;
        let model_input = serde_json::to_string(&body["input"])?;
        assert!(!model_input.contains("note [image]"));
        assert!(!model_input.contains("mailbox-original-image"));
        assert!(!model_input.contains("mailbox-original-mention"));
    }
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn archived_unloaded_user_mail_retries_keep_state_and_agent_keys_are_separate() -> Result<()>
{
    let (mut app, _home, server, store) = mailbox_app(MultiAgentVersion::V1).await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    // Explicit placement materializes a durable thread without sampling a model turn.
    let _: ThreadSectionMoveResponse = app
        .request(|request_id| ClientRequest::ThreadSectionMove {
            request_id,
            params: ThreadSectionMoveParams {
                thread_id: thread.id.clone(),
                section_id: None,
                before_thread_id: None,
            },
        })
        .await?;
    let _: ThreadArchiveResponse = app
        .request(|request_id| ClientRequest::ThreadArchive {
            request_id,
            params: ThreadArchiveParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(!loaded.data.contains(&thread.id));
    let receiver = ThreadId::from_string(&thread.id)?;
    let sender = ThreadId::new();
    let agent_key = serde_json::to_string(&("v1-send-input-mailbox", sender, "turn", "call"))?;
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let agent_mail = store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: agent_key.clone(),
            payload: MailboxPayload::Agent {
                input: vec![CoreUserInput::Text {
                    text: "separate agent mail".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(sender),
                    recipient: identity(receiver),
                    sender_turn_id: "turn".to_string(),
                }),
            },
        })
        .await?;
    let params = ThreadMailboxAddParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "offline user note".to_string(),
            text_elements: Vec::new(),
        }],
        client_user_message_id: agent_key.clone(),
    };
    let accepted = add_mail(&mut app, params.clone()).await?;
    assert_ne!(accepted.message_id, agent_mail.id);
    assert_eq!(add_mail(&mut app, params.clone()).await?, accepted);
    store
        .reject_mailbox_input(RejectMailboxInputParams {
            receiver_thread_id: receiver,
            message_id: accepted.message_id.clone(),
            reason: "fixture explicit rejection".to_string(),
        })
        .await?;
    assert_eq!(
        add_mail(&mut app, params.clone()).await?,
        ThreadMailboxAddResponse {
            message_id: accepted.message_id,
            state: ThreadMailboxMessageState::Rejected,
            rejection_reason: Some("fixture explicit rejection".to_string()),
        },
    );
    let mut changed = params;
    changed.input = vec![UserInput::Text {
        text: "different retry".to_string(),
        text_elements: Vec::new(),
    }];
    let request_id = app
        .send_request("thread/mailbox/add", Some(serde_json::to_value(changed)?))
        .await?;
    let error = timeout(
        READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert!(error.error.message.contains("different input"));
    assert_eq!(
        store.lookup_mailbox_input(receiver, &agent_key).await?,
        Some(agent_mail)
    );
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(!loaded.data.contains(&thread.id));
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

#[test_case(json!({"input": []}); "empty input")]
#[test_case(json!({"clientUserMessageId": ""}); "empty retry identity")]
#[test_case(json!({"author": "agent"}); "no client authority discriminator")]
#[test_case(json!({"attribution": {"sender": "forged"}}); "no client attribution")]
#[test_case(json!({"input": [{"type": "image", "url": "https://example.com/image.png"}]}); "existing remote media rejection")]
#[tokio::test]
async fn invalid_user_mailbox_requests_are_not_accepted(
    overrides: serde_json::Value,
) -> Result<()> {
    let (mut app, _home, server, store) = mailbox_app(MultiAgentVersion::V1).await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let mut params = json!({
        "threadId": thread.id, "input": [{"type":"text", "text":"must not be accepted"}],
        "clientUserMessageId": "invalid-mail",
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(overrides.as_object().unwrap().clone());
    let request_id = app.send_request("thread/mailbox/add", Some(params)).await?;
    timeout(
        READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        store
            .lookup_mailbox_input(
                ThreadId::from_string(&thread.id)?,
                &serde_json::to_string(&("user-mailbox", "invalid-mail"))?,
            )
            .await?,
        None,
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn user_mailbox_requires_experimental_handshake() -> Result<()> {
    let (mut app, home, _server, _store) = mailbox_app(MultiAgentVersion::V1).await?;
    app.shutdown_gracefully().await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build()
        .await?;
    app.initialize_with_capabilities(
        ClientInfo {
            name: "mailbox-gate".to_string(),
            title: None,
            version: "0.1".to_string(),
        },
        Some(InitializeCapabilities::default()),
    )
    .await?;
    let request_id = app.send_request("thread/mailbox/add", Some(json!({
        "threadId": ThreadId::new().to_string(), "input": [{"type":"text","text":"not admitted"}],
        "clientUserMessageId": "gate-test",
    }))).await?;
    let error = timeout(
        READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert!(error.error.message.contains("experimental"));
    app.shutdown_gracefully().await?;
    Ok(())
}

#[test_case(MultiAgentVersion::V2; "v2")]
#[test_case(MultiAgentVersion::Disabled; "disabled")]
#[tokio::test]
async fn loaded_unsupported_backend_rejects_mail_without_starting_a_turn(
    backend: MultiAgentVersion,
) -> Result<()> {
    let (mut app, _home, server, store) = mailbox_app(backend).await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let request_id = app
        .send_request(
            "thread/mailbox/add",
            Some(json!({
                "threadId": thread.id,
                "input": [{"type": "text", "text": "unsupported backend note"}],
                "clientUserMessageId": "unsupported-mail",
            })),
        )
        .await?;
    let error = timeout(
        READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert!(
        error
            .error
            .message
            .contains("does not support mailbox consumption")
    );
    assert_eq!(
        store
            .lookup_mailbox_input(
                ThreadId::from_string(&thread.id)?,
                &serde_json::to_string(&("user-mailbox", "unsupported-mail"))?,
            )
            .await?,
        None,
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

async fn mailbox_app(
    backend: MultiAgentVersion,
) -> Result<(TestAppServer, TempDir, MockServer, LocalThreadStore)> {
    let server =
        create_mock_responses_server_repeating_assistant("Inventory noted; do not consume.").await;
    let home = TempDir::new()?;
    let config = MockResponsesConfig::new(&server.uri());
    let config = match backend {
        MultiAgentVersion::V1 => config
            .enable_feature(Feature::Collab)
            .disable_feature(Feature::MultiAgentV2),
        MultiAgentVersion::V2 => config.enable_feature(Feature::MultiAgentV2),
        MultiAgentVersion::Disabled => config
            .disable_feature(Feature::Collab)
            .disable_feature(Feature::MultiAgentV2)
            .with_root_config("[agents]\nenabled = false"),
    };
    config.write(home.path())?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let state = StateRuntime::init(sqlite.clone(), "mock_provider".to_string()).await?;
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(),
            sqlite,
            default_model_provider_id: "mock_provider".to_string(),
        },
        Some(state),
    );
    let app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    Ok((app, home, server, store))
}

async fn add_mail(
    app: &mut TestAppServer,
    params: ThreadMailboxAddParams,
) -> Result<ThreadMailboxAddResponse> {
    app.request(|request_id| ClientRequest::ThreadMailboxAdd { request_id, params })
        .await
}
