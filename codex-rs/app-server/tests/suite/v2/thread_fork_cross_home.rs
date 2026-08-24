use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_fake_paginated_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::rollout_path;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::append_rollout_item_to_path;
use codex_rollout::read_session_meta_line;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::timeout;

use super::connection_handling_websocket::create_config_toml;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn thread_fork_copies_cross_home_paginated_lineage_by_path() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let source_home = TempDir::new()?;
    let active_home = TempDir::new()?;
    create_config_toml(active_home.path(), &server.uri(), "never")?;

    let root_message = "Cross-home paginated root";
    let root_id = create_fake_paginated_rollout(
        source_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        root_message,
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let root_path = rollout_path(source_home.path(), "2025-01-05T12-00-00", root_id.as_str());
    append_rollout_item_to_path(
        root_path.as_path(),
        &RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "root-turn".to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: Some(10),
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    )
    .await?;
    let root_position = HistoryPosition {
        thread_id: ThreadId::from_string(root_id.as_str())?,
        end_ordinal_exclusive: 4,
        end_byte_offset: std::fs::metadata(root_path.as_path())?.len(),
    };

    let child_message = "Cross-home paginated child";
    let child_id = create_fake_paginated_rollout(
        source_home.path(),
        "2025-01-05T12-00-01",
        "2025-01-05T12:00:01Z",
        child_message,
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let child_path = rollout_path(source_home.path(), "2025-01-05T12-00-01", child_id.as_str());
    let mut child_lines = std::fs::read_to_string(child_path.as_path())?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    child_lines[0]["payload"]["history_base"] = serde_json::to_value(root_position)?;
    for (index, line) in child_lines.iter_mut().enumerate() {
        line["ordinal"] = serde_json::to_value(index + 4)?;
    }
    std::fs::write(
        child_path.as_path(),
        format!(
            "{}\n",
            child_lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )?;
    append_rollout_item_to_path(
        child_path.as_path(),
        &RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "child-turn".to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: Some(20),
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    )
    .await?;
    let rolled_back_message = "Cross-home child message removed by exact rollback";
    append_rollout_item_to_path(
        child_path.as_path(),
        &RolloutItem::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: rolled_back_message.to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
    )
    .await?;
    append_rollout_item_to_path(
        child_path.as_path(),
        &RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            rollback_start_index: Some(4),
            ..Default::default()
        })),
    )
    .await?;
    let mut child_metadata_update = read_session_meta_line(child_path.as_path()).await?;
    child_metadata_update.meta.memory_mode = Some("enabled".to_string());
    child_metadata_update.meta.base_instructions = Some(BaseInstructions {
        text: "Inherited cross-home instructions".to_string(),
        provenance: None,
    });
    append_rollout_item_to_path(
        child_path.as_path(),
        &RolloutItem::SessionMeta(child_metadata_update),
    )
    .await?;
    let root_contents = std::fs::read_to_string(root_path.as_path())?;
    let child_contents = std::fs::read_to_string(child_path.as_path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(active_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let fork_id = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: child_id.clone(),
            path: Some(child_path.clone()),
            ..Default::default()
        })
        .await?;
    let ThreadForkResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(fork_id)).await??;

    assert_ne!(thread.id, child_id);
    assert_eq!(thread.forked_from_id, Some(child_id));
    assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
    let fork_path = thread.path.expect("forked rollout path");
    assert!(fork_path.starts_with(active_home.path()));
    let fork_meta = read_session_meta_line(fork_path.as_path()).await?;
    assert_eq!(fork_meta.meta.history_base, None);
    assert_eq!(
        fork_meta.meta.base_instructions,
        Some(BaseInstructions {
            text: "Inherited cross-home instructions".to_string(),
            provenance: None,
        })
    );
    let fork_contents = std::fs::read_to_string(fork_path.as_path())?;
    let fork_session_meta_count = fork_contents
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|line| matches!(line.item, RolloutItem::SessionMeta(_)))
        .count();
    assert_eq!(fork_session_meta_count, 1);
    assert!(fork_contents.contains(root_message));
    assert!(fork_contents.contains(child_message));
    assert!(!fork_contents.contains(rolled_back_message));
    assert_eq!(std::fs::read_to_string(root_path.as_path())?, root_contents);
    assert_eq!(
        std::fs::read_to_string(child_path.as_path())?,
        child_contents
    );

    let fork_thread_id = thread.id;
    timeout(DEFAULT_READ_TIMEOUT, mcp.shutdown_gracefully()).await??;
    let source_home_path = source_home.path().to_path_buf();
    source_home.close()?;
    assert!(!source_home_path.exists());

    let mut mcp = TestAppServer::builder()
        .with_codex_home(active_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: fork_thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(resume_id)).await??;

    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: fork_thread_id,
            input: vec![UserInput::Text {
                text: "Continue from the copied lineage".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_id)).await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let requests = server.received_requests().await.expect("response requests");
    let model_input = requests
        .iter()
        .find(|request| request.url.path().ends_with("/responses"))
        .expect("forked turn response request")
        .body_json::<Value>()?["input"]
        .to_string();
    assert!(model_input.contains(root_message));
    assert!(model_input.contains(child_message));
    assert!(!model_input.contains(rolled_back_message));
    assert!(model_input.contains("Continue from the copied lineage"));

    Ok(())
}
