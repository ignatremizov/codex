use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnloadParams;
use codex_app_server_protocol::ThreadUnloadResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[tokio::test]
async fn unload_preserves_history_and_releases_runtime_for_resume() -> Result<()> {
    let responses = create_mock_responses_server_repeating_assistant("Saved answer").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&responses.uri())
        .with_sandbox_mode("danger-full-access")
        .write(home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = server
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_id = thread.id;
    timeout(
        REQUEST_TIMEOUT,
        server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: "Save this conversation.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    let before: ThreadReadResponse = server
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread_id.clone(),
                include_turns: true,
            },
        })
        .await?;

    let request_id = server
        .send_request(
            "thread/unload",
            Some(serde_json::to_value(ThreadUnloadParams {
                thread_id: thread_id.clone(),
            })?),
        )
        .await?;
    let unloaded: ThreadUnloadResponse =
        timeout(REQUEST_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(
        unloaded,
        ThreadUnloadResponse {
            root_thread_id: thread_id.clone(),
            unloaded_thread_ids: vec![thread_id.clone()],
        }
    );
    let loaded: ThreadLoadedListResponse = server
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    assert_eq!(
        loaded,
        ThreadLoadedListResponse {
            data: Vec::new(),
            next_cursor: None,
        }
    );
    let after: ThreadReadResponse = server
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread_id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(after.thread.turns, before.thread.turns);
    let retry_id = server
        .send_request(
            "thread/unload",
            Some(serde_json::to_value(ThreadUnloadParams {
                thread_id: thread_id.clone(),
            })?),
        )
        .await?;
    let retried: ThreadUnloadResponse =
        timeout(REQUEST_TIMEOUT, server.read_response(retry_id)).await??;
    assert_eq!(
        retried,
        ThreadUnloadResponse {
            root_thread_id: thread_id.clone(),
            unloaded_thread_ids: Vec::new(),
        }
    );
    let resumed: ThreadResumeResponse = server
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(resumed.thread.id, thread_id);
    Ok(())
}
