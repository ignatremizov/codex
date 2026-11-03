use super::*;
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;
use serde_json::json;

#[derive(Clone, Copy)]
enum SteerSource {
    Local,
    External,
}

#[tokio::test]
async fn local_steers_wake_sleep_and_preserve_next_turn_work() -> anyhow::Result<()> {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        assert_steer_wakes_sleep(SteerSource::Local, mode).await?;
    }
    Ok(())
}

#[tokio::test]
async fn external_edited_steers_wake_sleep_and_preserve_next_turn_work() -> anyhow::Result<()> {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        assert_steer_wakes_sleep(SteerSource::External, mode).await?;
    }
    Ok(())
}

async fn assert_steer_wakes_sleep(
    source: SteerSource,
    history_mode: ThreadHistoryMode,
) -> anyhow::Result<()> {
    let server = start_mock_server().await;
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
            responses::sse_completed("after-steer"),
            responses::sse_completed("ordinary-followup"),
        ],
    )
    .await;
    let (installed, extensions) = registered_queue_extensions();
    let test = test_codex()
        .with_extensions(extensions)
        .with_history_mode(history_mode)
        .with_config(|config| config.include_environment_context = false)
        .with_model_info_override("queue-clock-model", |info| {
            info.experimental_supported_tools.push("clock".to_string());
        })
        .build_with_auto_env(&server)
        .await?;
    let thread_id = test.session_configured.thread_id;
    let queue = install_registered_queue(&test, installed.as_ref())?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::new(user_input("start sleeping")))
        .await?;
    let original_turn = wait_for_event_match(test.codex.as_ref(), |event| match event {
        EventMsg::ItemStarted(item) if item.item.id() == "sleep-call" => Some(item.turn_id.clone()),
        _ => None,
    })
    .await;
    queue
        .enqueue(thread_id, user_input("ordinary next turn"))
        .await?;
    assert_eq!(1, model.requests().len());

    match source {
        SteerSource::Local => {
            queue
                .enqueue_steer(thread_id, structured_user_input("wake this turn"))
                .await?;
        }
        SteerSource::External => {
            let runtime = StateRuntime::init(
                test.codex
                    .state_db()
                    .context("state runtime unavailable")?
                    .sqlite()
                    .clone(),
                "test-provider".to_string(),
            )
            .await?;
            let external = QueuedItemService::new(
                Arc::new(LocalQueueStore::new(runtime)),
                Weak::new(),
                Arc::new(NoopExtensionEventSink),
            );
            let item = external
                .enqueue_steer(thread_id, structured_user_input("before edit"))
                .await?;
            let edited = external
                .update(thread_id, item.id, user_input("wake this turn"))
                .await?
                .context("steer disappeared before dispatch")?;
            assert_eq!(structured_user_input("wake this turn"), edited.input);
            let mut registry = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
            codex_queue_extension::install(&mut registry, Arc::clone(&queue));
            advance_queue_poll().await;
        }
    }
    let sleep = wait_for_event_match(test.codex.as_ref(), |event| match event {
        EventMsg::ItemCompleted(item) if item.item.id() == "sleep-call" => {
            Some((item.turn_id.clone(), item.item.clone()))
        }
        _ => None,
    })
    .await;
    let sleep_json = serde_json::to_value(sleep.1)?;
    assert_eq!(
        json!({"turn": sleep.0, "outcome": sleep_json["outcome"]}),
        json!({"turn": original_turn, "outcome": "interrupted"})
    );
    for _ in 0..2 {
        wait_for_event_with_timeout(
            test.codex.as_ref(),
            |event| matches!(event, EventMsg::TurnComplete(_)),
            Duration::from_secs(/*secs*/ 10),
        )
        .await;
    }
    wait_for_queue_empty(&queue, thread_id).await?;
    let requests = model.requests();
    assert_eq!(3, requests.len());
    assert_eq!(
        vec!["start sleeping", "wake this turn"],
        requests[1].message_input_texts("user")
    );
    assert!(
        requests[1]
            .function_call_output_text("sleep-call")
            .context("missing sleep output")?
            .contains("Sleep interrupted by new input.")
    );
    assert_eq!(
        Some("ordinary next turn"),
        requests[2]
            .message_input_texts("user")
            .last()
            .map(String::as_str)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_steer_survives_reconstruction_and_starts_when_idle() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let model = responses::mount_sse_once(&server, responses::sse_completed("steered-idle")).await;
    let admission = Arc::new(TestAdmission(AtomicBool::new(true)));
    let mut registry = ExtensionRegistryBuilder::new();
    registry.turn_start_admission(admission.clone());
    let test = test_codex()
        .with_extensions(Arc::new(registry.build()))
        .with_config(|config| config.include_environment_context = false)
        .build_with_auto_env(&server)
        .await?;
    let thread_id = test.session_configured.thread_id;
    let store = loaded_thread_queue(&test)?;
    let staged = QueuedItemService::new(
        Arc::clone(&store),
        Weak::new(),
        Arc::new(NoopExtensionEventSink),
    );
    let item = staged
        .enqueue_steer(thread_id, structured_user_input("retained steer"))
        .await?;
    let loaded = QueuedItemService::new(
        store,
        Arc::downgrade(&test.thread_manager),
        Arc::new(NoopExtensionEventSink),
    );
    emit_idle(&loaded, thread_id).await;
    assert_eq!(vec![item], loaded.list(thread_id).await?);
    assert!(model.requests().is_empty());
    admission.0.store(false, Ordering::SeqCst);
    emit_idle(&loaded, thread_id).await;
    wait_for_event_with_timeout(
        test.codex.as_ref(),
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(/*secs*/ 10),
    )
    .await;
    wait_for_queue_empty(&loaded, thread_id).await?;
    assert_eq!(
        vec!["retained steer"],
        model.single_request().message_input_texts("user")
    );
    Ok(())
}
