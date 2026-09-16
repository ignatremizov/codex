use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::create_config_toml;
use super::connection_handling_websocket::read_error_for_id;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::send_initialize_request;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;
use anyhow::Result;
use app_test_support::create_fake_rollout_with_text_elements;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadUnloadParams;
use codex_app_server_protocol::ThreadUnloadResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn another_subscriber_prevents_unload_without_stopping_the_actor() -> Result<()> {
    let responses = create_mock_responses_server_repeating_assistant("Still running").await;
    let home = tempfile::tempdir()?;
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
        let denied = read_error_for_id(&mut first, /*id*/ 11).await?;
        assert_eq!(denied.error.code, -32600);
        assert!(denied.error.message.contains("another subscribed client"));

        // A rejected unload must not have closed ordinary actor admission.
        send_request(
            &mut first,
            "turn/start",
            /*id*/ 12,
            Some(serde_json::to_value(TurnStartParams {
                thread_id: thread_id.clone(),
                input: vec![UserInput::Text {
                    text: "Continue after the denied unload.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })?),
        )
        .await?;
        let _: TurnStartResponse = to_response(read_response_for_id(&mut first, /*id*/ 12).await?)?;
        send_request(
            &mut second,
            "thread/unsubscribe",
            /*id*/ 13,
            Some(serde_json::to_value(ThreadUnsubscribeParams {
                thread_id: thread_id.clone(),
            })?),
        )
        .await?;
        let _: ThreadUnsubscribeResponse =
            to_response(read_response_for_id(&mut second, /*id*/ 13).await?)?;
        send_request(
            &mut first,
            "thread/unload",
            /*id*/ 14,
            Some(serde_json::to_value(ThreadUnloadParams {
                thread_id: thread_id.clone(),
            })?),
        )
        .await?;
        let unloaded: ThreadUnloadResponse =
            to_response(read_response_for_id(&mut first, /*id*/ 14).await?)?;
        assert_eq!(
            unloaded,
            ThreadUnloadResponse {
                root_thread_id: thread_id.clone(),
                unloaded_thread_ids: vec![thread_id],
            }
        );
        Ok(())
    }
    .await;
    process.kill().await?;
    result
}
