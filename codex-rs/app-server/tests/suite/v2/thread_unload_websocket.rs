use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::create_config_toml;
use super::connection_handling_websocket::read_jsonrpc_message;
use super::connection_handling_websocket::read_notification_for_method;
use super::connection_handling_websocket::read_response_and_notification_for_method;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::send_initialize_request;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use app_test_support::create_fake_rollout_with_text_elements;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnloadParams;
use codex_app_server_protocol::ThreadUnloadResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case("thread/start"; "fresh_root")]
#[test_case("thread/resume"; "cold_resume")]
#[tokio::test]
async fn unrelated_connected_client_does_not_prevent_unload(method: &str) -> Result<()> {
    let responses = create_mock_responses_server_repeating_assistant("Unused").await;
    let home = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    create_config_toml(home.path(), &responses.uri(), "never")?;
    let saved_thread_id = create_fake_rollout_with_text_elements(
        home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved question",
        Vec::new(),
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let (mut process, address) = spawn_websocket_server(home.path()).await?;
    let result = async {
        let mut first = connect_websocket(address).await?;
        let mut second = connect_websocket(address).await?;
        for (id, client) in [(1, &mut first), (2, &mut second)] {
            send_initialize_request(client, id, "independent-unload-test").await?;
            read_response_for_id(client, id).await?;
        }
        // These requests only load idle runtimes, without starting model turns or executors.
        send_request(
            &mut second,
            "thread/start",
            /*id*/ 3,
            Some(serde_json::to_value(ThreadStartParams {
                cwd: Some(project.path().display().to_string()),
                ..Default::default()
            })?),
        )
        .await?;
        let other: ThreadStartResponse =
            to_response(read_response_for_id(&mut second, /*id*/ 3).await?)?;

        let params = if method == "thread/start" {
            serde_json::to_value(ThreadStartParams {
                cwd: Some(project.path().display().to_string()),
                ..Default::default()
            })?
        } else {
            serde_json::to_value(ThreadResumeParams {
                thread_id: saved_thread_id,
                cwd: Some(project.path().display().to_string()),
                ..Default::default()
            })?
        };
        send_request(&mut first, method, /*id*/ 4, Some(params)).await?;
        let response = read_response_for_id(&mut first, /*id*/ 4).await?;
        let thread_id = if method == "thread/start" {
            to_response::<ThreadStartResponse>(response)?.thread.id
        } else {
            to_response::<ThreadResumeResponse>(response)?.thread.id
        };
        send_request(
            &mut first,
            "thread/unload",
            /*id*/ 5,
            Some(serde_json::to_value(ThreadUnloadParams {
                thread_id: thread_id.clone(),
            })?),
        )
        .await?;
        let unloaded: ThreadUnloadResponse = loop {
            match read_jsonrpc_message(&mut first).await? {
                JSONRPCMessage::Response(response) if response.id == RequestId::Integer(5) => {
                    break to_response(response)?;
                }
                JSONRPCMessage::Error(error) if error.id == RequestId::Integer(5) => {
                    bail!("independent thread unload failed: {}", error.error.message);
                }
                _ => {}
            }
        };
        assert_eq!(
            unloaded,
            ThreadUnloadResponse {
                root_thread_id: thread_id.clone(),
                unloaded_thread_ids: vec![thread_id],
            }
        );

        send_request(
            &mut second,
            "thread/loaded/list",
            /*id*/ 6,
            Some(serde_json::to_value(ThreadLoadedListParams::default())?),
        )
        .await?;
        let loaded: ThreadLoadedListResponse =
            to_response(read_response_for_id(&mut second, /*id*/ 6).await?)?;
        assert_eq!(
            loaded,
            ThreadLoadedListResponse {
                data: vec![other.thread.id],
                next_cursor: None,
            }
        );
        Ok(())
    }
    .await;
    process.kill().await?;
    result
}

#[tokio::test]
async fn unload_with_another_subscriber_releases_writer_for_another_server() -> Result<()> {
    let responses =
        create_mock_responses_server_repeating_assistant("Continued after handoff").await;
    let home = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    create_config_toml(home.path(), &responses.uri(), "never")?;
    let thread_id = create_fake_rollout_with_text_elements(
        home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved question",
        Vec::new(),
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let (mut process, address) = spawn_websocket_server(home.path()).await?;
    let (mut next_process, next_address) = spawn_websocket_server(home.path()).await?;
    let result = async {
        let mut first = connect_websocket(address).await?;
        let mut second = connect_websocket(address).await?;
        for (id, client) in [(1, &mut first), (2, &mut second)] {
            send_initialize_request(client, id, "unload-test").await?;
            read_response_for_id(client, id).await?;
            send_request(
                client,
                "thread/resume",
                /*id*/ 10,
                Some(serde_json::to_value(ThreadResumeParams {
                    thread_id: thread_id.clone(),
                    cwd: Some(project.path().display().to_string()),
                    ..Default::default()
                })?),
            )
            .await?;
            let _: ThreadResumeResponse =
                to_response(read_response_for_id(client, /*id*/ 10).await?)?;
        }
        send_request(
            &mut first,
            "thread/unload",
            /*id*/ 11,
            Some(serde_json::to_value(ThreadUnloadParams {
                thread_id: thread_id.clone(),
            })?),
        )
        .await?;
        let (response, first_closed) =
            read_response_and_notification_for_method(&mut first, /*id*/ 11, "thread/closed")
                .await?;
        let unloaded: ThreadUnloadResponse = to_response(response)?;
        assert_eq!(
            unloaded,
            ThreadUnloadResponse {
                root_thread_id: thread_id.clone(),
                unloaded_thread_ids: vec![thread_id.clone()],
            }
        );
        let second_closed = read_notification_for_method(&mut second, "thread/closed").await?;
        let expected_closed = Some(serde_json::json!({ "threadId": thread_id }));
        assert_eq!(
            (first_closed.params, second_closed.params),
            (expected_closed.clone(), expected_closed)
        );

        send_request(
            &mut second,
            "thread/loaded/list",
            /*id*/ 12,
            Some(serde_json::to_value(ThreadLoadedListParams::default())?),
        )
        .await?;
        let loaded: ThreadLoadedListResponse =
            to_response(read_response_for_id(&mut second, /*id*/ 12).await?)?;
        assert_eq!(
            loaded,
            ThreadLoadedListResponse {
                data: Vec::new(),
                next_cursor: None,
            }
        );

        // Keep both original clients and their server alive across the handoff. The new
        // server must acquire the same rollout writer without waiting for them to exit.
        let mut next = connect_websocket(next_address).await?;
        send_initialize_request(&mut next, /*id*/ 1, "handoff-test").await?;
        read_response_for_id(&mut next, /*id*/ 1).await?;
        send_request(
            &mut next,
            "thread/resume",
            /*id*/ 10,
            Some(serde_json::to_value(ThreadResumeParams {
                thread_id: thread_id.clone(),
                cwd: Some(project.path().display().to_string()),
                ..Default::default()
            })?),
        )
        .await?;
        let resumed: ThreadResumeResponse =
            to_response(read_response_for_id(&mut next, /*id*/ 10).await?)?;
        assert_eq!(resumed.thread.id, thread_id);

        send_request(
            &mut next,
            "turn/start",
            /*id*/ 11,
            Some(serde_json::to_value(TurnStartParams {
                thread_id: thread_id.clone(),
                input: vec![UserInput::Text {
                    text: "Continue under the new server.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })?),
        )
        .await?;
        let (response, completed) =
            read_response_and_notification_for_method(&mut next, /*id*/ 11, "turn/completed")
                .await?;
        let started: TurnStartResponse = to_response(response)?;
        let completed: TurnCompletedNotification =
            serde_json::from_value(completed.params.context("missing turn/completed params")?)?;
        assert_eq!(
            (
                completed.thread_id,
                completed.turn.id,
                completed.turn.status
            ),
            (thread_id, started.turn.id, TurnStatus::Completed)
        );
        Ok(())
    }
    .await;
    process.kill().await?;
    next_process.kill().await?;
    result
}
