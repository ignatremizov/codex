//! Shared evidence must survive the host-owned agent and Guardian deletion callbacks.

use super::DEFAULT_READ_TIMEOUT;
use super::board_snapshot;
use super::create_delete_test_rollout;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_core::find_thread_path_by_id_str;
use codex_features::Feature;
use codex_guardian_v3::local_board_options as guardian_board_options;
use codex_message_board::LocalBoardOptions;
use codex_message_board::configure_local_board;
use codex_protocol::ThreadId;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

#[test_case::test_case(false; "guardian_disabled")]
#[test_case::test_case(true; "guardian_enabled")]
#[tokio::test]
async fn thread_delete_preserves_guardian_evidence(guardian_v3_enabled: bool) -> Result<()> {
    let home = TempDir::new()?;
    let owner = create_delete_test_rollout(home.path(), /*minute*/ 0, "Owner")?;
    let retained = create_delete_test_rollout(home.path(), /*minute*/ 1, "Retained")?;
    let child = create_delete_test_rollout(home.path(), /*minute*/ 2, "Child")?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let options = guardian_board_options();
    for id in [&owner, &retained] {
        configure_local_board(
            &sqlite,
            &options,
            ThreadId::from_string(id)?.into(),
            serde_json::from_value(json!({
                "template": {"version": 1, "channels": {"evidence": {}}}
            }))?,
        )
        .await?;
    }
    let pool = sqlite
        .open_read_only_pool(
            &home.path().join("message_board_guardian_1.sqlite"),
            /*busy_timeout*/ None,
        )
        .await?;
    let before = board_snapshot(&pool).await?;
    let templates_before: Vec<String> =
        sqlx::query_scalar("SELECT board FROM board_templates ORDER BY board")
            .fetch_all(&pool)
            .await?;
    assert_eq!(templates_before.len(), 2);
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::AgentMessageBoard)
        .with_extra_config(&format!(
            "[features.guardianv3]\nenabled = {guardian_v3_enabled}"
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let state = StateRuntime::init(sqlite, "mock_provider".into()).await?;
    state
        .upsert_thread_spawn_edge(
            ThreadId::from_string(&owner)?,
            ThreadId::from_string(&child)?,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await?;
    // A child is not the disposal owner of its surviving main thread's evidence.
    let _: ThreadDeleteResponse = app
        .request(|request_id| ClientRequest::ThreadDelete {
            request_id,
            params: ThreadDeleteParams {
                thread_id: child.clone(),
            },
        })
        .await?;
    assert_eq!(
        find_thread_path_by_id_str(home.path(), &child, /*state_db_ctx*/ None).await?,
        None
    );
    assert_eq!(board_snapshot(&pool).await?, before);
    if guardian_v3_enabled {
        let request_id = app
            .send_thread_delete_request(ThreadDeleteParams {
                thread_id: owner.clone(),
            })
            .await?;
        let error = timeout(
            DEFAULT_READ_TIMEOUT,
            app.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.error.code, -32603);
        assert!(error.error.message.contains("exclusive cleanup ownership"));
    } else {
        // Disabled Guardian cleanup does not inspect or dispose its persisted store.
        let _: ThreadDeleteResponse = app
            .request(|request_id| ClientRequest::ThreadDelete {
                request_id,
                params: ThreadDeleteParams {
                    thread_id: owner.clone(),
                },
            })
            .await?;
    }
    assert_eq!(
        find_thread_path_by_id_str(home.path(), &owner, /*state_db_ctx*/ None)
            .await?
            .is_some(),
        guardian_v3_enabled
    );
    assert!(
        find_thread_path_by_id_str(home.path(), &retained, /*state_db_ctx*/ None)
            .await?
            .is_some()
    );
    assert_eq!(board_snapshot(&pool).await?, before);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT board FROM board_templates ORDER BY board")
            .fetch_all(&pool)
            .await?,
        templates_before
    );
    pool.close().await;
    app.shutdown_gracefully().await?;
    state.close().await;
    Ok(())
}

#[test_case::test_case("agent_message_board_1.sqlite", false, false; "agent_open_failure")]
#[test_case::test_case("agent_message_board_1.sqlite", true, false; "agent_open_failure_with_guardian")]
#[test_case::test_case("message_board_guardian_1.sqlite", true, false; "guardian_open_failure")]
#[test_case::test_case("agent_message_board_1.sqlite", false, true; "agent_corrupt")]
#[test_case::test_case("agent_message_board_1.sqlite", true, true; "agent_corrupt_with_guardian")]
#[test_case::test_case("message_board_guardian_1.sqlite", true, true; "guardian_corrupt")]
#[tokio::test]
async fn thread_delete_does_not_recover_or_mutate_shared_board_storage(
    database_file: &str,
    guardian_v3_enabled: bool,
    corrupt_file: bool,
) -> Result<()> {
    let home = TempDir::new()?;
    let selected = create_delete_test_rollout(home.path(), /*minute*/ 0, "Delete me")?;
    let retained = create_delete_test_rollout(home.path(), /*minute*/ 1, "Keep me")?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let (other_options, other_name) = if database_file == "agent_message_board_1.sqlite" {
        (guardian_board_options(), "message_board_guardian_1.sqlite")
    } else {
        (
            LocalBoardOptions {
                storage_namespace: "agent_message_board".into(),
            },
            "agent_message_board_1.sqlite",
        )
    };
    configure_local_board(
        &sqlite,
        &other_options,
        ThreadId::from_string(&retained)?.into(),
        serde_json::from_value(json!({
            "template": {"version": 1, "channels": {"evidence": {}}}
        }))?,
    )
    .await?;
    let other_pool = sqlite
        .open_read_only_pool(&home.path().join(other_name), /*busy_timeout*/ None)
        .await?;
    let before = board_snapshot(&other_pool).await?;
    let database = home.path().join(database_file);
    let corrupt = b"not a SQLite database";
    if corrupt_file {
        std::fs::write(&database, corrupt)?;
    } else {
        std::fs::create_dir(&database)?;
    }
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::AgentMessageBoard)
        .with_extra_config(&format!(
            "[features.guardianv3]\nenabled = {guardian_v3_enabled}"
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let request_id = app
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: selected.clone(),
        })
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32603);
    assert!(
        error
            .error
            .message
            .contains("cleanup blocked thread deletion")
    );
    if corrupt_file {
        assert_eq!(std::fs::read(&database)?, corrupt);
    } else {
        assert!(database.is_dir());
    }
    assert!(!home.path().join("db-backups").exists());
    assert_eq!(board_snapshot(&other_pool).await?, before);
    for id in [&selected, &retained] {
        assert!(
            find_thread_path_by_id_str(home.path(), id, /*state_db_ctx*/ None)
                .await?
                .is_some()
        );
    }
    other_pool.close().await;
    app.shutdown_gracefully().await?;
    Ok(())
}
