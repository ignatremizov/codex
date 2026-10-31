use super::DEFAULT_READ_TIMEOUT;
use super::compaction_config;
use super::wait_for_context_compaction_completed;
use super::wait_for_context_compaction_started;
use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolFunctionSpec;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_start_during_a_tool_waits_and_continues_the_original_turn(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_function_call("analysis", "analysis_gate", "{}"),
                responses::ev_completed("analysis"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("checkpoint", "Keep the completed analysis."),
                responses::ev_completed("compacted"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("continued", "Finished using the analysis."),
                responses::ev_completed("continued"),
            ]),
        ],
    )
    .await;
    let home = TempDir::new()?;
    compaction_config(&server.uri(), /*auto_compact_limit*/ 1_000_000).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let start_id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(history_mode),
            dynamic_tools: Some(vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "analysis_gate".into(),
                description: "Finish outstanding analysis.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                defer_loading: false,
            })]),
            ..Default::default()
        })
        .await?;
    let started: ThreadStartResponse =
        timeout(DEFAULT_READ_TIMEOUT, app.read_response(start_id)).await??;
    let thread_id = started.thread.id;
    let start_turn_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: "Finish the task after analyzing the screenshots.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn: TurnStartResponse =
        timeout(DEFAULT_READ_TIMEOUT, app.read_response(start_turn_id)).await??;
    let tool = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::DynamicToolCall { request_id, params } = tool else {
        anyhow::bail!("expected an outstanding tool: {tool:?}");
    };
    assert_eq!(
        (params.thread_id, params.turn_id),
        (thread_id.clone(), turn.turn.id.clone())
    );
    for _ in 0..2 {
        let compact_id = app
            .send_thread_compact_start_request(ThreadCompactStartParams {
                thread_id: thread_id.clone(),
            })
            .await?;
        let _: ThreadCompactStartResponse =
            timeout(DEFAULT_READ_TIMEOUT, app.read_response(compact_id)).await??;
    }
    assert_eq!(captured.requests().len(), 1);
    app.send_response(
        request_id,
        serde_json::to_value(DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "Useful screenshot finding.".into(),
            }],
            success: true,
        })?,
    )
    .await?;
    let compact_started = wait_for_context_compaction_started(&mut app).await?;
    let compact_completed = wait_for_context_compaction_completed(&mut app).await?;
    let completed: TurnCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(
        (
            compact_started.turn_id,
            compact_completed.turn_id,
            completed.turn.id,
            completed.turn.status,
            completed.turn.error
        ),
        (
            turn.turn.id.clone(),
            turn.turn.id.clone(),
            turn.turn.id,
            TurnStatus::Completed,
            None
        )
    );
    assert_eq!(captured.requests().len(), 3);
    Ok(())
}
