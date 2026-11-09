use super::*;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadQueueResultParams;
use codex_app_server_protocol::ThreadQueueResultResponse;
use codex_app_server_protocol::ThreadQueueScheduleParams;
use codex_app_server_protocol::ThreadUnloadParams;
use codex_app_server_protocol::ThreadUnloadResponse;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn future_input_does_not_block_ordinary_input_and_can_be_edited_or_cancelled(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("ordinary"),
            responses::ev_assistant_message("answer", "ordinary completed"),
            responses::ev_completed("ordinary"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread;
    let due = chrono::Utc::now().timestamp() + 3600;
    let scheduled = schedule(&mut app, &thread.id, "schedule-id", "later", due).await?;
    assert_eq!(scheduled.id, "schedule-id");
    let edited: ThreadQueueUpdateResponse = app
        .request(|request_id| ClientRequest::ThreadQueueUpdate {
            request_id,
            params: ThreadQueueUpdateParams {
                thread_id: thread.id.clone(),
                queued_submission_id: scheduled.id.clone(),
                input: vec![text("edited later")],
            },
        })
        .await?;
    assert_eq!(edited.queued_submission.scheduled_at, Some(due));
    queue_item(&mut app, submission(&thread.id, "ordinary now")).await?;
    let completed: TurnCompletedNotification =
        timeout(READ_TIMEOUT, app.read_notification("turn/completed")).await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(
        model
            .single_request()
            .message_input_text_groups("user")
            .last(),
        Some(&vec!["ordinary now".to_string()])
    );
    assert_eq!(
        list_queue(&mut app, &thread.id).await?.data,
        vec![edited.queued_submission]
    );
    let cancelled: ThreadQueueDeleteResponse = app
        .request(|request_id| ClientRequest::ThreadQueueDelete {
            request_id,
            params: ThreadQueueDeleteParams {
                thread_id: thread.id.clone(),
                queued_submission_id: scheduled.id,
            },
        })
        .await?;
    assert!(cancelled.deleted);
    Ok(())
}

#[derive(Clone, Copy)]
enum Residency {
    Loaded,
    Unloaded,
}

#[test_case(ThreadHistoryMode::Legacy, Residency::Loaded; "loaded legacy")]
#[test_case(ThreadHistoryMode::Legacy, Residency::Unloaded; "unloaded legacy")]
#[test_case(ThreadHistoryMode::Paginated, Residency::Loaded; "loaded paginated")]
#[test_case(ThreadHistoryMode::Paginated, Residency::Unloaded; "unloaded paginated")]
#[tokio::test]
async fn due_input_dispatches_without_a_new_queue_write_and_publishes_an_exact_receipt(
    history_mode: ThreadHistoryMode,
    residency: Residency,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("scheduled"),
            responses::ev_assistant_message("answer", "scheduled completed"),
            responses::ev_completed("scheduled"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread;
    if matches!(residency, Residency::Unloaded) {
        let _: ThreadUnloadResponse = app
            .request(|request_id| ClientRequest::ThreadUnload {
                request_id,
                params: ThreadUnloadParams {
                    thread_id: thread.id.clone(),
                },
            })
            .await?;
    }
    schedule(
        &mut app,
        &thread.id,
        "scheduled-input",
        "run headlessly",
        chrono::Utc::now().timestamp() + 1,
    )
    .await?;
    let result = timeout(Duration::from_secs(/*secs*/ 40), async {
        loop {
            let result: ThreadQueueResultResponse = app
                .request(|request_id| ClientRequest::ThreadQueueResult {
                    request_id,
                    params: ThreadQueueResultParams {
                        queued_submission_id: "scheduled-input".into(),
                    },
                })
                .await?;
            if result
                .turn
                .as_ref()
                .is_some_and(|turn| turn.status != TurnStatus::InProgress)
            {
                assert_eq!(
                    result.turn.as_ref().map(|turn| &turn.status),
                    Some(&TurnStatus::Completed)
                );
                break anyhow::Ok(result);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 100)).await;
        }
    })
    .await??;
    assert_eq!(
        (result.thread_id, result.started, result.queued_submission),
        (Some(thread.id), Some(true), None),
    );
    assert_eq!(
        model
            .single_request()
            .message_input_text_groups("user")
            .last(),
        Some(&vec!["run headlessly".to_string()])
    );
    let turn = result.turn.context("receiving turn missing")?;
    assert_eq!(result.turn_id, Some(turn.id));
    assert!(turn.items.iter().any(|item| matches!(&item.payload, ThreadItemPayload::AgentMessage { text, .. } if text == "scheduled completed")));
    Ok(())
}

async fn schedule(
    app: &mut TestAppServer,
    thread_id: &str,
    id: &str,
    prompt: &str,
    at: i64,
) -> Result<QueuedSubmission> {
    let response: ThreadQueueAddResponse = app
        .request(|request_id| ClientRequest::ThreadQueueSchedule {
            request_id,
            params: ThreadQueueScheduleParams {
                thread_id: thread_id.into(),
                input: vec![text(prompt)],
                client_user_message_id: id.into(),
                scheduled_at: at,
            },
        })
        .await?;
    Ok(response.queued_submission)
}

#[tokio::test]
async fn cancellation_remains_available_after_archiving_the_target() -> Result<()> {
    let server = responses::start_mock_server().await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let item = schedule(
        &mut app,
        &thread.id,
        "cancel-archived",
        "future input",
        chrono::Utc::now().timestamp() + 3600,
    )
    .await?;
    let _: ThreadArchiveResponse = app
        .request(|request_id| ClientRequest::ThreadArchive {
            request_id,
            params: ThreadArchiveParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    let result: ThreadQueueResultResponse = app
        .request(|request_id| ClientRequest::ThreadQueueResult {
            request_id,
            params: ThreadQueueResultParams {
                queued_submission_id: item.id.clone(),
            },
        })
        .await?;
    assert_eq!(result.queued_submission, Some(item.clone()));
    let removed: ThreadQueueDeleteResponse = app
        .request(|request_id| ClientRequest::ThreadQueueDelete {
            request_id,
            params: ThreadQueueDeleteParams {
                thread_id: thread.id,
                queued_submission_id: item.id,
            },
        })
        .await?;
    assert_eq!(removed, ThreadQueueDeleteResponse { deleted: true });
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn due_input_starts_an_interrupted_loaded_thread_under_a_new_turn(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("sleeping"),
                responses::ev_function_call_with_namespace(
                    "sleep-call",
                    "clock",
                    "sleep",
                    &json!({ "duration_ms": 60_000 }).to_string(),
                ),
                responses::ev_completed("sleeping"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("scheduled"),
                responses::ev_assistant_message("answer", "resumed by schedule"),
                responses::ev_completed("scheduled"),
            ]),
        ],
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_extra_config("[features.sleep_tool]\nenabled = true\nmode = \"always_on\"")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread;
    let started: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![text("sleep")],
                ..Default::default()
            },
        })
        .await?;
    loop {
        let item: ItemStartedNotification =
            timeout(READ_TIMEOUT, app.read_notification("item/started")).await??;
        if matches!(&item.item.payload, ThreadItemPayload::Sleep(_)) {
            break;
        }
    }
    let _: TurnInterruptResponse = app
        .request(|request_id| ClientRequest::TurnInterrupt {
            request_id,
            params: TurnInterruptParams {
                thread_id: thread.id.clone(),
                turn_id: started.turn.id.clone(),
            },
        })
        .await?;
    let interrupted: TurnCompletedNotification =
        timeout(READ_TIMEOUT, app.read_notification("turn/completed")).await??;
    assert_eq!(interrupted.turn.status, TurnStatus::Interrupted);
    schedule(
        &mut app,
        &thread.id,
        "after-interrupt",
        "run after interrupt",
        chrono::Utc::now().timestamp(),
    )
    .await?;
    let result = timeout(Duration::from_secs(/*secs*/ 40), async {
        loop {
            let result: ThreadQueueResultResponse = app
                .request(|request_id| ClientRequest::ThreadQueueResult {
                    request_id,
                    params: ThreadQueueResultParams {
                        queued_submission_id: "after-interrupt".into(),
                    },
                })
                .await?;
            if result
                .turn
                .as_ref()
                .is_some_and(|turn| turn.status != TurnStatus::InProgress)
            {
                assert_eq!(
                    result.turn.as_ref().map(|turn| &turn.status),
                    Some(&TurnStatus::Completed)
                );
                break anyhow::Ok(result);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 100)).await;
        }
    })
    .await??;
    assert_eq!(result.started, Some(true));
    assert_ne!(result.turn_id, Some(started.turn.id));
    assert_eq!(model.requests().len(), 2);
    Ok(())
}
