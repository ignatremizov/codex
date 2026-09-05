use std::collections::BTreeMap;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ExperimentalFeatureEnablementSetParams;
use codex_app_server_protocol::ExperimentalFeatureEnablementSetResponse;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::io::Write;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn runtime_enabled_legacy_migration_preserves_model_context_and_decode_error() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_assistant_message("msg-1", "legacy assistant message"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-2"),
                responses::ev_assistant_message("msg-2", "resumed assistant message"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;

    let mut primary = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let start_id = primary
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Legacy),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, primary.read_response(start_id)).await??;
    let rollout_path = thread.path.clone().expect("materialized rollout path");
    timeout(
        DEFAULT_READ_TIMEOUT,
        primary.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "legacy user message".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    timeout(DEFAULT_READ_TIMEOUT, primary.shutdown_gracefully()).await??;
    let mut rollout = std::fs::OpenOptions::new()
        .append(true)
        .open(rollout_path)?;
    let line = RolloutLine {
        timestamp: "2025-01-05T12:00:01Z".to_string(),
        ordinal: None,
        item: RolloutItem::EventMsg(EventMsg::ContextCompacted(ContextCompactedEvent {
            summary: None,
            message: None,
            decode_error: Some("handoff decoder unavailable".to_string()),
            available_skills: Vec::new(),
        })),
    };
    writeln!(rollout, "{}", serde_json::to_string(&line)?)?;
    drop(rollout);

    let mut secondary = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let enablement_id = secondary
        .send_experimental_feature_enablement_set_request(ExperimentalFeatureEnablementSetParams {
            enablement: BTreeMap::from([(
                "background_paginated_rollout_migration".to_string(),
                true,
            )]),
        })
        .await?;
    let _: ExperimentalFeatureEnablementSetResponse =
        timeout(DEFAULT_READ_TIMEOUT, secondary.read_response(enablement_id)).await??;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let read_id = secondary
                .send_thread_read_request(ThreadReadParams {
                    thread_id: thread.id.clone(),
                    include_turns: false,
                })
                .await?;
            let ThreadReadResponse { thread: read } = secondary.read_response(read_id).await?;
            if read.history_mode == ThreadHistoryMode::Paginated {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;

    let resume_id = secondary
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse {
        thread: resumed, ..
    } = timeout(DEFAULT_READ_TIMEOUT, secondary.read_response(resume_id)).await??;
    assert_eq!(resumed.history_mode, ThreadHistoryMode::Paginated);

    let turns_id = secondary
        .send_thread_turns_list_request(ThreadTurnsListParams {
            thread_id: thread.id.clone(),
            cursor: None,
            limit: Some(10),
            sort_direction: Some(SortDirection::Asc),
            items_view: Some(TurnItemsView::Full),
        })
        .await?;
    let ThreadTurnsListResponse { data, .. } =
        timeout(DEFAULT_READ_TIMEOUT, secondary.read_response(turns_id)).await??;
    let compactions = data
        .into_iter()
        .flat_map(|turn| turn.items)
        .filter_map(|item| match item {
            ThreadItem::ContextCompaction {
                summary,
                message,
                decode_error,
                ..
            } => Some((summary, message, decode_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        compactions,
        vec![(None, None, Some("handoff decoder unavailable".to_string()))]
    );

    timeout(
        DEFAULT_READ_TIMEOUT,
        secondary.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id,
            input: vec![UserInput::Text {
                text: "resumed user message".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let resumed_request = requests.last().expect("resumed turn request");
    let user_messages = resumed_request.message_input_texts("user");
    assert!(user_messages.contains(&"legacy user message".to_string()));
    assert!(user_messages.contains(&"resumed user message".to_string()));
    assert!(resumed_request.body_contains_text("legacy assistant message"));

    Ok(())
}
