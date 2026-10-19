use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_command_execution_sse_response;
use codex_app_server_protocol::TerminalInteractionNotification;
use codex_app_server_protocol::TerminalWait;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadResumeInitialTurnsPageParams;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_target_windows;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Request;
use wiremock::matchers::method;
use wiremock::matchers::path;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn loaded_resume_reemits_current_wait_with_original_identity_and_deadline(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_target_windows!(Ok(()), "fixture uses the POSIX sleep command");
    let server = responses::start_mock_server().await;
    let exec_response = responses::mount_sse_once(
        &server,
        create_command_execution_sse_response(
            vec!["sleep".into(), "120".into()],
            /*workdir*/ None,
            /*timeout_ms*/ Some(1_000),
            "command",
        )?,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(|request: &Request| {
            responses::body_contains(request, "Process running with session ID")
        })
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(
                &responses::request_body_bytes(request).expect("decode model request"),
            )
            .expect("model request JSON");
            let output = body["input"]
                .as_array()
                .expect("model input array")
                .iter()
                .find(|item| item["type"] == "function_call_output" && item["call_id"] == "command")
                .and_then(|item| item["output"].as_str())
                .expect("command tool output");
            let process_id: i32 = output
                .lines()
                .find_map(|line| line.strip_prefix("Process running with session ID "))
                .expect("background process identity")
                .parse()
                .expect("numeric process identity");
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("wait-response"),
                responses::ev_function_call(
                    "wait",
                    "write_stdin",
                    &json!({"session_id": process_id, "yield_time_ms": 600_000}).to_string(),
                ),
                responses::ev_completed("wait-response"),
            ]))
        })
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_sandbox_mode("danger-full-access")
        .write(home.path())?;
    let mut client = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let request_id = client
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(READ_TIMEOUT, client.read_response(request_id)).await??;
    let request_id = client
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Start a command and wait for it.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(READ_TIMEOUT, client.read_response(request_id)).await??;
    let started: TerminalInteractionNotification = timeout(
        READ_TIMEOUT,
        client.read_notification("item/commandExecution/terminalInteraction"),
    )
    .await??;
    assert!(matches!(
        started.wait,
        Some(TerminalWait::Started { ref interaction_id, .. }) if interaction_id == "wait"
    ));
    assert!(started.deadline_at_ms.is_some());

    let request_id = client
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            exclude_turns: true,
            initial_turns_page: Some(ThreadResumeInitialTurnsPageParams {
                limit: Some(1),
                sort_direction: None,
                items_view: Some(TurnItemsView::NotLoaded),
            }),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = timeout(READ_TIMEOUT, client.read_response(request_id)).await??;
    let restored: TerminalInteractionNotification = timeout(
        READ_TIMEOUT,
        client.read_notification("item/commandExecution/terminalInteraction"),
    )
    .await??;
    assert_eq!(restored, started);
    assert!(
        exec_response
            .single_request()
            .message_input_texts("user")
            .iter()
            .any(|text| text == "Start a command and wait for it.")
    );
    client
        .interrupt_turn_and_wait_for_aborted(thread.id, turn.id, READ_TIMEOUT)
        .await?;
    Ok(())
}
