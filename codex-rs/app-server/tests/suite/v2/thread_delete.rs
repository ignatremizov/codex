use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_fake_paginated_rollout;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadDeletedNotification;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_core::find_thread_path_by_id_str;
use codex_features::Feature;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_state::AgentAliasAllocation;
use codex_state::AgentAliasRecord;
use codex_state::AgentAliasState;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_delete_rejects_paginated_writer_owned_by_another_process() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let thread_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2025-01-01T00-00-00",
        "2025-01-01T00:00:00Z",
        "owned",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let mut owner = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let _: ThreadResumeResponse = owner
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: thread_id.clone(),
                exclude_turns: true,
                ..Default::default()
            },
        })
        .await?;

    let mut other = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let request_id = other
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        other.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert_eq!(
        error.error.message,
        format!("thread {thread_id} already has an active writer")
    );
    timeout(DEFAULT_READ_TIMEOUT, owner.shutdown_gracefully()).await??;
    let _: ThreadDeleteResponse = other
        .request(|request_id| ClientRequest::ThreadDelete {
            request_id,
            params: ThreadDeleteParams { thread_id },
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn thread_delete_does_not_touch_a_related_thread_owned_by_another_process() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let parent_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2025-01-01T00-00-00",
        "2025-01-01T00:00:00Z",
        "parent",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let child_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2025-01-01T00-01-00",
        "2025-01-01T00:01:00Z",
        "child",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let parent_thread_id = ThreadId::from_string(&parent_id)?;
    let child_thread_id = ThreadId::from_string(&child_id)?;
    let mut child_owner = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let _: ThreadResumeResponse = child_owner
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: child_id.clone(),
                exclude_turns: true,
                ..Default::default()
            },
        })
        .await?;

    let state_db = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    state_db
        .upsert_thread_spawn_edge(
            parent_thread_id,
            child_thread_id,
            DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await?;

    let mut deleter = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let _: ThreadDeleteResponse = deleter
        .request(|request_id| ClientRequest::ThreadDelete {
            request_id,
            params: ThreadDeleteParams {
                thread_id: parent_id.clone(),
            },
        })
        .await?;
    let deleted: ThreadDeletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        deleter.read_notification("thread/deleted"),
    )
    .await??;
    assert_eq!(deleted.thread_id, parent_id);

    assert!(
        find_thread_path_by_id_str(
            codex_home.path(),
            child_id.as_str(),
            /*state_db_ctx*/ None,
        )
        .await?
        .is_some()
    );
    let ThreadLoadedListResponse { data, .. } = child_owner
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    assert_eq!(data, vec![child_id]);

    timeout(DEFAULT_READ_TIMEOUT, child_owner.shutdown_gracefully()).await??;
    Ok(())
}

#[tokio::test]
async fn thread_delete_preserves_spawned_descendants_and_their_graph() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;

    let parent_id = create_delete_test_rollout(codex_home.path(), /*minute*/ 0, "parent")?;
    let parent_thread_id = ThreadId::from_string(&parent_id)?;
    let session_id = SessionId::from(parent_thread_id);
    let child_id = create_delete_test_agent_rollout(
        codex_home.path(),
        /*minute*/ 1,
        "child",
        session_id,
        parent_thread_id,
        /*depth*/ 1,
        "Child",
    )?;
    let child_thread_id = ThreadId::from_string(&child_id)?;
    let grandchild_id = create_delete_test_agent_rollout(
        codex_home.path(),
        /*minute*/ 2,
        "grandchild",
        session_id,
        child_thread_id,
        /*depth*/ 2,
        "Grandchild",
    )?;
    let grandchild_thread_id = ThreadId::from_string(&grandchild_id)?;

    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    state_db.ensure_agent_alias_namespace(session_id).await?;
    for (parent, child, nickname) in [
        (parent_thread_id, child_thread_id, "Child"),
        (child_thread_id, grandchild_thread_id, "Grandchild"),
    ] {
        state_db
            .allocate_agent_alias(AgentAliasAllocation {
                session_id,
                parent_thread_id: parent,
                child_thread_id: child,
                nickname: Some(nickname.to_string()),
            })
            .await?;
    }

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let _: ThreadDeleteResponse = mcp
        .request(|request_id| ClientRequest::ThreadDelete {
            request_id,
            params: ThreadDeleteParams {
                thread_id: parent_id.clone(),
            },
        })
        .await?;

    let deleted_notification: ThreadDeletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("thread/deleted"),
    )
    .await??;
    assert_eq!(deleted_notification.thread_id, parent_id);

    let parent_rollout_path = find_thread_path_by_id_str(
        codex_home.path(),
        &parent_thread_id.to_string(),
        /*state_db_ctx*/ None,
    )
    .await?;
    assert_eq!(parent_rollout_path, None);
    for thread_id in [child_thread_id, grandchild_thread_id] {
        let rollout_path = find_thread_path_by_id_str(
            codex_home.path(),
            &thread_id.to_string(),
            /*state_db_ctx*/ None,
        )
        .await?;
        assert!(
            rollout_path.is_some(),
            "expected related rollout for {thread_id} to remain"
        );
    }
    assert_eq!(
        state_db
            .list_thread_spawn_descendants(parent_thread_id)
            .await?,
        vec![child_thread_id, grandchild_thread_id]
    );
    assert_eq!(
        state_db
            .find_current_agent_alias_by_thread(child_thread_id)
            .await?,
        Some(AgentAliasRecord {
            session_id,
            thread_id: child_thread_id,
            agent_ref: 2,
            nickname: Some("Child".to_string()),
            state: AgentAliasState::Active,
        })
    );

    let resumed: ThreadResumeResponse = mcp
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: child_id,
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(resumed.thread.id, child_thread_id.to_string());
    Ok(())
}

#[tokio::test]
async fn thread_delete_preflights_external_fork_references_without_touching_related_threads()
-> Result<()> {
    let codex_home = TempDir::new()?;

    let parent_id = create_delete_test_rollout(codex_home.path(), /*minute*/ 0, "parent")?;
    let child_id = create_delete_test_rollout(codex_home.path(), /*minute*/ 1, "child")?;
    let external_id = create_delete_test_rollout(codex_home.path(), /*minute*/ 2, "external")?;
    let parent_thread_id = ThreadId::from_string(&parent_id)?;
    let child_thread_id = ThreadId::from_string(&child_id)?;
    let external_thread_id = ThreadId::from_string(&external_id)?;
    let parent_path = find_thread_path_by_id_str(
        codex_home.path(),
        &parent_thread_id.to_string(),
        /*state_db_ctx*/ None,
    )
    .await?
    .expect("parent rollout path");
    let external_path = find_thread_path_by_id_str(
        codex_home.path(),
        &external_thread_id.to_string(),
        /*state_db_ctx*/ None,
    )
    .await?
    .expect("external rollout path");
    let mut external_meta: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(external_path.as_path())?
            .lines()
            .next()
            .expect("external session metadata"),
    )?;
    external_meta["payload"]["history_base"] = serde_json::to_value(HistoryPosition {
        thread_id: parent_thread_id,
        end_ordinal_exclusive: 1,
        end_byte_offset: std::fs::metadata(parent_path.as_path())?.len(),
    })?;
    std::fs::write(external_path.as_path(), format!("{external_meta}\n"))?;

    let state_db = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    state_db
        .upsert_thread_spawn_edge(
            parent_thread_id,
            child_thread_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;

    let delete_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: parent_id.clone(),
        })
        .await?;
    let delete_err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(delete_id)),
    )
    .await??;
    assert_eq!(
        delete_err.error.message,
        format!("cannot delete thread {parent_thread_id}: forked history still references it")
    );

    for thread_id in [parent_thread_id, child_thread_id, external_thread_id] {
        assert!(
            find_thread_path_by_id_str(
                codex_home.path(),
                &thread_id.to_string(),
                /*state_db_ctx*/ None,
            )
            .await?
            .is_some(),
            "expected rollout for {thread_id} to remain"
        );
    }
    assert_eq!(
        state_db
            .list_thread_spawn_descendants(parent_thread_id)
            .await?,
        vec![child_thread_id]
    );
    Ok(())
}

fn create_delete_test_rollout(codex_home: &Path, minute: u8, preview: &str) -> Result<String> {
    create_fake_rollout(
        codex_home,
        &format!("2025-01-01T00-{minute:02}-00"),
        &format!("2025-01-01T00:{minute:02}:00Z"),
        preview,
        Some("mock_provider"),
        /*git_info*/ None,
    )
}

fn create_delete_test_agent_rollout(
    codex_home: &Path,
    minute: u8,
    preview: &str,
    session_id: SessionId,
    parent_thread_id: ThreadId,
    depth: i32,
    nickname: &str,
) -> Result<String> {
    create_fake_parented_rollout_with_source(
        codex_home,
        &format!("2025-01-01T00-{minute:02}-00"),
        &format!("2025-01-01T00:{minute:02}:00Z"),
        preview,
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_path: None,
            agent_nickname: Some(nickname.to_string()),
            agent_role: None,
        }),
        session_id,
        parent_thread_id,
    )
}

#[tokio::test]
async fn thread_delete_handles_live_threads_before_rollout_exists() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let persisted_thread = mcp.start_thread(ThreadStartParams::default()).await?.thread;
    let rollout_path = find_thread_path_by_id_str(
        codex_home.path(),
        &persisted_thread.id,
        /*state_db_ctx*/ None,
    )
    .await?;
    assert_eq!(rollout_path, None);

    let _: ThreadDeleteResponse = mcp
        .request(|request_id| ClientRequest::ThreadDelete {
            request_id,
            params: ThreadDeleteParams {
                thread_id: persisted_thread.id,
            },
        })
        .await?;

    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            ephemeral: Some(true),
            ..Default::default()
        })
        .await?;

    let delete_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let delete_err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(delete_id)),
    )
    .await??;
    let expected_message = format!(
        "thread is not persisted and cannot be deleted: {}",
        thread.id
    );
    assert_eq!(delete_err.error.message, expected_message);

    let ThreadLoadedListResponse { mut data, .. } = mcp
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    data.sort();
    assert_eq!(data, vec![thread.id]);

    Ok(())
}

#[tokio::test]
async fn thread_delete_preserves_boards_even_with_feature_disabled() -> Result<()> {
    let home = TempDir::new()?;
    let thread_ids = seed_delete_test_boards(home.path()).await?;
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let pool = sqlite
        .open_read_only_pool(
            &sqlite.home().join("agent_message_board_1.sqlite"),
            /*busy_timeout*/ None,
        )
        .await?;
    let before = board_snapshot(&pool).await?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::AgentMessageBoard)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let request_id = app
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: thread_ids[0].clone(),
        })
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32603);
    assert!(error.error.message.contains("exclusive cleanup ownership"));
    for thread_id in &thread_ids {
        assert!(
            find_thread_path_by_id_str(home.path(), thread_id, /*state_db_ctx*/ None)
                .await?
                .is_some()
        );
    }
    // A thread without board records needs no destructive cleanup, even though
    // unrelated roots have populated boards in the same database.
    let empty_id = create_delete_test_rollout(home.path(), /*minute*/ 2, "No board records")?;
    let _: ThreadDeleteResponse = app
        .request(|request_id| ClientRequest::ThreadDelete {
            request_id,
            params: ThreadDeleteParams {
                thread_id: empty_id.clone(),
            },
        })
        .await?;
    assert_eq!(
        find_thread_path_by_id_str(home.path(), &empty_id, /*state_db_ctx*/ None).await?,
        None
    );
    assert_eq!(board_snapshot(&pool).await?, before);
    pool.close().await;
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn thread_delete_preserves_corrupt_shared_board_with_either_feature_setting() -> Result<()> {
    for enabled in [false, true] {
        let home = TempDir::new()?;
        let thread_ids = seed_delete_test_boards(home.path()).await?;
        let sqlite = SqliteConfig::new_for_testing(home.path().abs());
        let database = home.path().join("agent_message_board_1.sqlite");
        let pool = sqlite.open_read_write_pool(&database).await?;
        let before = board_snapshot(&pool).await?;
        let checkpoint: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&pool)
            .await?;
        assert_eq!(checkpoint, (0, 0, 0));
        pool.close().await;
        let original = std::fs::read(&database)?;
        let recovery_dir = home.path().join("db-backups").join("preserved-fixture");
        std::fs::create_dir_all(&recovery_dir)?;
        let recovery_file = recovery_dir.join("agent_message_board_1.sqlite");
        std::fs::write(&recovery_file, &original)?;
        let mut corrupt = original.clone();
        corrupt[..16].fill(/*value*/ 0);
        std::fs::write(&database, &corrupt)?;

        let server = create_mock_responses_server_repeating_assistant("Done").await;
        let config = MockResponsesConfig::new(&server.uri());
        let config = if enabled {
            config
                .enable_feature(Feature::AgentMessageBoard)
                .enable_feature(Feature::MultiAgentV2)
        } else {
            config.disable_feature(Feature::AgentMessageBoard)
        };
        config.write(home.path())?;
        let mut app = TestAppServer::builder()
            .with_codex_home(home.path())
            .build_initialized()
            .await?;
        let state = StateRuntime::init(sqlite.clone(), "mock_provider".into()).await?;
        let mut saved_threads = Vec::new();
        for id in &thread_ids {
            let thread_id = ThreadId::from_string(id)?;
            let rollout = find_thread_path_by_id_str(home.path(), id, /*state_db_ctx*/ None)
                .await?
                .expect("seeded rollout");
            let metadata = state.get_thread(thread_id).await?.expect("seeded state");
            saved_threads.push((thread_id, std::fs::read(&rollout)?, rollout, metadata));
        }
        let request_id = app
            .send_thread_delete_request(ThreadDeleteParams {
                thread_id: thread_ids[0].clone(),
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
        assert_eq!(std::fs::read(&database)?, corrupt);
        assert_eq!(std::fs::read(&recovery_file)?, original);
        assert_eq!(
            std::fs::read_dir(home.path().join("db-backups"))?.count(),
            1
        );
        assert_eq!(std::fs::read_dir(&recovery_dir)?.count(), 1);
        for (id, bytes, rollout, metadata) in &saved_threads {
            assert_eq!(std::fs::read(rollout)?, *bytes);
            assert_eq!(state.get_thread(*id).await?, Some(metadata.clone()));
        }
        app.shutdown_gracefully().await?;

        // Explicit fixture restoration is not a recovery side effect of deletion.
        // Verify both roots' complete records and subscriptions before retrying.
        std::fs::copy(&recovery_file, &database)?;
        let pool = sqlite
            .open_read_only_pool(&database, /*busy_timeout*/ None)
            .await?;
        assert_eq!(board_snapshot(&pool).await?, before);
        let mut app = TestAppServer::builder()
            .with_codex_home(home.path())
            .build_initialized()
            .await?;
        let request_id = app
            .send_thread_delete_request(ThreadDeleteParams {
                thread_id: thread_ids[0].clone(),
            })
            .await?;
        let error = timeout(
            DEFAULT_READ_TIMEOUT,
            app.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.error.code, -32603);
        assert!(error.error.message.contains("exclusive cleanup ownership"));
        assert_eq!(board_snapshot(&pool).await?, before);
        for (id, bytes, rollout, metadata) in &saved_threads {
            assert_eq!(std::fs::read(rollout)?, *bytes);
            assert_eq!(state.get_thread(*id).await?, Some(metadata.clone()));
        }
        pool.close().await;
        state.close().await;
        app.shutdown_gracefully().await?;
    }
    Ok(())
}

async fn seed_delete_test_boards(home: &Path) -> Result<Vec<String>> {
    let mut events = Vec::new();
    for label in ["A", "B"] {
        events.extend([
            responses::sse(vec![
                responses::ev_function_call_with_namespace(
                    label,
                    "collaboration",
                    "post",
                    &json!({"new_channel_name":"design", "text":format!("Decision {label}")})
                        .to_string(),
                ),
                responses::ev_completed(label),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("done", "Done"),
                responses::ev_completed("done"),
            ]),
        ]);
    }
    let server = create_mock_responses_server_sequence(events).await;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::AgentMessageBoard)
        .enable_feature(Feature::MultiAgentV2)
        .write(home)?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home)
        .build_initialized()
        .await?;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
        let completed = app
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Post the decision".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        assert_eq!(completed.turn.status, TurnStatus::Completed);
        ids.push(thread.id);
    }
    app.shutdown_gracefully().await?;
    let sqlite = SqliteConfig::new_for_testing(home.abs());
    let pool = sqlite
        .open_read_only_pool(
            &home.join("agent_message_board_1.sqlite"),
            /*busy_timeout*/ None,
        )
        .await?;
    let snapshot = board_snapshot(&pool).await?;
    assert_eq!(
        snapshot.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 2, 2, 0]
    );
    pool.close().await;
    Ok(ids)
}

async fn board_snapshot(pool: &sqlx::SqlitePool) -> Result<Vec<Vec<serde_json::Value>>> {
    let mut snapshot = Vec::new();
    for query in [
        "SELECT json_group_array(json_array(board,name,name_search,created_at,timestamp,author))
         FROM (SELECT * FROM channels ORDER BY board,name)",
        "SELECT json_group_array(json_array(seq,board,id,channel,root,author,timestamp,
                                           body_search,payload,request_id,request))
         FROM (SELECT * FROM posts ORDER BY seq)",
        "SELECT json_group_array(json_array(board,target,agent))
         FROM (SELECT * FROM subscriptions ORDER BY board,target,agent)",
        "SELECT json_group_array(board) FROM (SELECT board FROM deleted_boards ORDER BY board)",
    ] {
        let rows: String = sqlx::query_scalar(query).fetch_one(pool).await?;
        snapshot.push(serde_json::from_str(&rows)?);
    }
    Ok(snapshot)
}
