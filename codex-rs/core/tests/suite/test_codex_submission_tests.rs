//! Turn helpers follow admission receipts, not whichever start event is next in the queue.

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Config;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(10);

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_turn_does_not_complete_from_an_earlier_turns_queued_events(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let mut first = server
        .mount_response(
            |request| request.body_contains_text("first helper turn"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![ev_completed("first-response")]),
            }],
        )
        .await;
    let (release, gate) = oneshot::channel();
    let mut second = server
        .mount_response(
            |request| request.body_contains_text("second helper turn"),
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![ev_completed("second-response")]),
            }],
        )
        .await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let initial = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "first helper turn".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started {
        turn_id: initial_turn_id,
    } = initial
    else {
        anyhow::bail!("initial turn must start: {initial:?}");
    };
    timeout(TIMEOUT, first.wait_for_request()).await?;
    // This consumes no protocol events: the first turn's start and completion
    // deliberately remain queued when the helper submits the second turn.
    ThreadIdle::wait(&test.codex).await;

    let submission = test.submit_turn("second helper turn");
    tokio::pin!(submission);
    let request = timeout(TIMEOUT, async {
        tokio::select! {
            biased;
            result = &mut submission => anyhow::bail!("helper returned before its gated turn completed: {result:?}"),
            request = second.wait_for_request() => Ok(request),
        }
    }).await??;
    assert!(
        timeout(Duration::from_millis(100), submission.as_mut())
            .await
            .is_err(),
        "queued completion events cannot finish the helper while its own response is gated",
    );
    let body = request.body_json();
    let second_turn_id = body["client_metadata"]["turn_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("second request turn ID")?;
    assert_ne!(second_turn_id, initial_turn_id);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("second response gate closed"))?;
    timeout(TIMEOUT, submission).await??;
    ThreadIdle::wait(&test.codex).await;
    assert_eq!(server.requests().await.len(), 2);
    test.codex.shutdown_and_wait().await?;
    server.shutdown().await;
    Ok(())
}

#[derive(Default)]
struct NextConfigChange(Mutex<Option<oneshot::Sender<()>>>);

impl ConfigContributor<Config> for NextConfigChange {
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
        _previous_config: &Config,
        _new_config: &Config,
    ) {
        if let Some(entered) = self.0.lock().expect("config-change signal").take() {
            let _ = entered.send(());
        }
    }
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_turn_waits_for_the_steered_turn_without_another_start_event(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let mut first = server
        .mount_response(
            |request| request.body_contains_text("keep helper turn active"),
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![ev_completed("busy-response")]),
            }],
        )
        .await;
    let mut followup = server
        .mount_response(
            |request| request.body_contains_text("steer through helper"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("steered-response"),
                    ev_assistant_message("steered-final", "steering handled"),
                    ev_completed("steered-response"),
                ]),
            }],
        )
        .await;
    let changed = Arc::new(NextConfigChange::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.config_contributor(changed.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let initial = test
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "keep helper turn active".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::OnRequest),
                ..Default::default()
            }),
        )
        .await?;
    let TurnInputSubmission::Started { turn_id } = initial else {
        anyhow::bail!("initial turn must start: {initial:?}");
    };
    timeout(TIMEOUT, first.wait_for_request()).await?;
    assert_eq!(
        wait_for_event_match(&test.codex, |event| match event {
            EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
            _ => None,
        })
        .await,
        turn_id
    );

    // The helper changes OnRequest to Never only after steer_input has accepted
    // its input. This signal proves admission while the initial response is still
    // gated, without relying on an arbitrary sleep or a single poll of admission.
    let (entered, admitted) = oneshot::channel();
    *changed.0.lock().expect("arm config-change signal") = Some(entered);
    let submission = test.submit_turn("steer through helper");
    tokio::pin!(submission);
    timeout(TIMEOUT, async {
        tokio::select! {
            biased;
            result = &mut submission => anyhow::bail!("helper returned before its steered turn completed: {result:?}"),
            entered = admitted => { entered?; Ok(()) },
        }
    }).await??;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("initial response gate closed"))?;
    timeout(TIMEOUT, submission).await??;
    let request = timeout(TIMEOUT, followup.wait_for_request()).await?;
    assert_eq!(
        request.body_json()["client_metadata"]["turn_id"].as_str(),
        Some(turn_id.as_str())
    );
    ThreadIdle::wait(&test.codex).await;
    assert_eq!(server.requests().await.len(), 2);
    test.codex.shutdown_and_wait().await?;
    server.shutdown().await;
    Ok(())
}
