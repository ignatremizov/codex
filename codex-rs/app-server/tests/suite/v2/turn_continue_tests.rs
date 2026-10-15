use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnContinueParams;
use codex_app_server_protocol::TurnContinueResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn continue_restarts_original_turn_without_new_model_input() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "continuation currently certifies only thread-configured local environments"
    );
    let home = TempDir::new()?;
    let cwd = TempDir::new()?;
    let responses = responses::start_mock_server().await;
    let interrupted_request = responses::mount_sse_once_with_delay(
        &responses,
        responses::sse_completed("interrupted"),
        Duration::from_secs(/*secs*/ 60),
    )
    .await;
    MockResponsesConfig::new(&responses.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .start_thread(ThreadStartParams {
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "finish the original request".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let started: TurnStartedNotification = timeout(
        Duration::from_secs(/*secs*/ 10),
        app.read_notification("turn/started"),
    )
    .await??;
    assert_eq!(
        (started.thread_id, started.turn.id, started.turn.status),
        (thread.id.clone(), turn.id.clone(), TurnStatus::InProgress)
    );
    // Request capture establishes that original input/settings are already recorded.
    timeout(Duration::from_secs(/*secs*/ 10), async {
        while interrupted_request.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    let _: TurnInterruptResponse = app
        .request(|request_id| ClientRequest::TurnInterrupt {
            request_id,
            params: TurnInterruptParams {
                thread_id: thread.id.clone(),
                turn_id: turn.id.clone(),
            },
        })
        .await?;
    let interrupted: TurnCompletedNotification = timeout(
        Duration::from_secs(/*secs*/ 10),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(
        (
            interrupted.thread_id,
            interrupted.turn.id,
            interrupted.turn.status
        ),
        (thread.id.clone(), turn.id.clone(), TurnStatus::Interrupted)
    );
    let resumed_request =
        responses::mount_sse_once(&responses, responses::sse_completed("continued")).await;
    let continued: TurnContinueResponse = app
        .request(|request_id| ClientRequest::TurnContinue {
            request_id,
            params: TurnContinueParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    assert_eq!(
        continued,
        TurnContinueResponse {
            turn_id: turn.id.clone()
        }
    );
    let restarted: TurnStartedNotification = timeout(
        Duration::from_secs(/*secs*/ 10),
        app.read_notification("turn/started"),
    )
    .await??;
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(/*secs*/ 10),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(
        (
            restarted.thread_id,
            restarted.turn.id,
            restarted.turn.status
        ),
        (thread.id.clone(), turn.id.clone(), TurnStatus::InProgress)
    );
    assert_eq!(
        (
            completed.thread_id,
            completed.turn.id,
            completed.turn.status
        ),
        (thread.id, turn.id, TurnStatus::Completed)
    );
    let original = interrupted_request.single_request();
    let resumed = resumed_request.single_request();
    assert_eq!(
        resumed
            .input()
            .into_iter()
            .filter(|item| item["role"] == "user")
            .collect::<Vec<_>>(),
        original
            .input()
            .into_iter()
            .filter(|item| item["role"] == "user")
            .collect::<Vec<_>>()
    );
    assert!(!resumed.body_contains_text("<turn_aborted>"));
    assert!(!resumed.body_contains_text("generic.turn_aborted"));
    Ok(())
}

#[tokio::test]
async fn continue_rejects_empty_and_completed_threads() -> Result<()> {
    let home = TempDir::new()?;
    let responses = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    MockResponsesConfig::new(&responses.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app.start_thread(ThreadStartParams::default()).await?;

    for completed in [false, true] {
        if completed {
            let _: TurnStartResponse = app
                .request(|request_id| ClientRequest::TurnStart {
                    request_id,
                    params: TurnStartParams {
                        thread_id: thread.id.clone(),
                        input: vec![UserInput::Text {
                            text: "say done".into(),
                            text_elements: Vec::new(),
                        }],
                        ..Default::default()
                    },
                })
                .await?;
            let _: TurnCompletedNotification = timeout(
                Duration::from_secs(/*secs*/ 10),
                app.read_notification("turn/completed"),
            )
            .await??;
        }
        let request_id = app
            .send_request("turn/continue", Some(json!({ "threadId": thread.id })))
            .await?;
        let error: JSONRPCError = timeout(
            Duration::from_secs(/*secs*/ 10),
            app.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.error.code, -32600);
        assert!(error.error.message.contains("cannot continue turn"));
    }
    Ok(())
}
