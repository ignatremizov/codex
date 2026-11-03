use super::*;
use codex_app_server_protocol::ItemCompletedNotification;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn cross_profile_steer_wakes_sleep_using_the_hosting_threads_auth(
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
                    &json!({"duration_ms": 60_000}).to_string(),
                ),
                responses::ev_completed("sleeping"),
            ]),
            responses::sse_completed("awake"),
        ],
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config(
            "cli_auth_credentials_store = \"file\"\ninclude_environment_context = false",
        )
        .with_provider_config("env_key = \"QUEUE_TEST_TOKEN\"\nsupports_websockets = false")
        .with_extra_config("[features.sleep_tool]\nenabled = true\nmode = \"always_on\"")
        .write(home.path())?;
    let mut host = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[
            ("CODEX_AUTH_FILE", Some("auth-host.json")),
            ("QUEUE_TEST_TOKEN", Some("host-token")),
        ])
        .without_managed_config()
        .build_initialized()
        .await?;
    let mut sender = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[
            ("CODEX_AUTH_FILE", Some("auth-sender.json")),
            ("QUEUE_TEST_TOKEN", Some("sender-token")),
        ])
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = host
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread;
    let started: TurnStartResponse = host
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![text("start sleeping")],
                ..Default::default()
            },
        })
        .await?;
    loop {
        let item: ItemStartedNotification =
            timeout(READ_TIMEOUT, host.read_notification("item/started")).await??;
        if matches!(item.item, ThreadItem::Sleep(_)) {
            break;
        }
    }
    let accepted: ThreadQueueAddResponse = sender
        .request(|request_id| ClientRequest::ThreadQueueSteer {
            request_id,
            params: submission(&thread.id, "cross-profile wake"),
        })
        .await?;
    assert_eq!(
        vec![text("cross-profile wake")],
        accepted.queued_submission.input
    );
    let sleep = timeout(Duration::from_secs(/*secs*/ 25), async {
        loop {
            let item: ItemCompletedNotification = host.read_notification("item/completed").await?;
            if matches!(item.item, ThreadItem::Sleep(_)) {
                break anyhow::Ok(item);
            }
        }
    })
    .await??;
    let ThreadItem::Sleep(item) = sleep.item else {
        anyhow::bail!("expected a completed sleep");
    };
    assert_eq!(
        json!({"turn": sleep.turn_id, "outcome": item.outcome}),
        json!({"turn": started.turn.id, "outcome": "interrupted"})
    );
    let completed: TurnCompletedNotification =
        timeout(READ_TIMEOUT, host.read_notification("turn/completed")).await??;
    assert_eq!(started.turn.id, completed.turn.id);
    let requests = model.requests();
    assert_eq!(
        vec![
            Some("Bearer host-token".to_string()),
            Some("Bearer host-token".to_string())
        ],
        requests
            .iter()
            .map(|request| request.header("authorization"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        vec!["start sleeping", "cross-profile wake"],
        requests[1].message_input_texts("user")
    );
    assert!(
        requests[1]
            .function_call_output_text("sleep-call")
            .context("missing sleep result")?
            .contains("Sleep interrupted by new input.")
    );
    Ok(())
}
