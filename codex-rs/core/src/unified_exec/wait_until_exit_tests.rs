use super::UnifiedExecError;
use super::UserInputWait;
use super::tests;
use codex_login::CodexAuth;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalWaitCompletionReason;
use codex_protocol::protocol::TerminalWaitEvent;
use core_test_support::skip_if_sandbox;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
enum ObservedWaitPhase {
    Started,
    Finished(TerminalWaitCompletionReason),
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_before_wait_start_returns_an_error_without_a_lifecycle_pair() -> anyhow::Result<()>
{
    let (session, turn, mut events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |_| {},
        )
        .await;

    let error = tests::write_stdin_with_options(
        &session,
        &turn,
        /*process_id*/ 999_999,
        "",
        /*yield_time_ms*/ 1,
        /*wait_until_exit*/ true,
        CancellationToken::new(),
        /*user_input_wait*/ None,
        Some("missing-process-wait"),
    )
    .await
    .expect_err("unknown process should remain a user-visible write_stdin error");
    assert!(matches!(
        error,
        UnifiedExecError::UnknownProcessId {
            process_id: 999_999
        }
    ));

    let mut lifecycle_events = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::TerminalInteraction(event) = event.msg {
            lifecycle_events.push(event);
        }
    }
    assert!(
        lifecycle_events.is_empty(),
        "a wait that never began must not report a fabricated lifecycle: {lifecycle_events:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_until_exit_waits_release_on_input_and_cancellation_without_terminating()
-> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));

    let (session, turn, mut events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| config.background_terminal_max_timeout = Some(1_000),
        )
        .await;
    let (first, second) = tokio::join!(
        tests::exec_command(
            &session, &turn, "sleep 30", /*yield_time_ms*/ 100, /*workdir*/ None,
        ),
        tests::exec_command(
            &session, &turn, "sleep 3", /*yield_time_ms*/ 100, /*workdir*/ None,
        ),
    );
    let first_process_id = first
        .expect("first background process should start")
        .process_id
        .expect("first process should remain alive after initial yield");
    let second_process_id = second
        .expect("second background process should start")
        .process_id
        .expect("second process should remain alive after initial yield");

    let (activity_tx, steer_activity_rx) = watch::channel(/*init*/ 0);
    let input_cancellation = CancellationToken::new();
    let input_session = Arc::clone(&session);
    let input_turn = Arc::clone(&turn);
    let input_wait = tokio::spawn(async move {
        tests::write_stdin_with_options(
            &input_session,
            &input_turn,
            first_process_id,
            "",
            /*yield_time_ms*/ 1,
            /*wait_until_exit*/ true,
            input_cancellation,
            Some(UserInputWait {
                steer_activity_rx,
                pending_steer: false,
            }),
            Some("wait-input"),
        )
        .await
    });

    let exit_session = Arc::clone(&session);
    let exit_turn = Arc::clone(&turn);
    let exit_wait = tokio::spawn(async move {
        tests::write_stdin_with_options(
            &exit_session,
            &exit_turn,
            second_process_id,
            "",
            /*yield_time_ms*/ 1,
            /*wait_until_exit*/ true,
            CancellationToken::new(),
            /*user_input_wait*/ None,
            Some("wait-exit"),
        )
        .await
    });

    let started_waits = wait_for_wait_phases(
        &mut events,
        &["wait-input", "wait-exit"],
        /*started*/ true,
    )
    .await;
    assert_eq!(
        started_waits.get("wait-input"),
        Some(&ObservedWaitPhase::Started)
    );
    assert_eq!(
        started_waits.get("wait-exit"),
        Some(&ObservedWaitPhase::Started)
    );
    activity_tx.send_replace(1);

    let (input_response, exit_response) = tokio::join!(input_wait, exit_wait);
    let input_response = input_response
        .expect("input wait task should finish")
        .expect("input wait should return its current process state");
    let exit_response = exit_response
        .expect("exit wait task should finish")
        .expect("exit wait should return its completed process state");
    let finished_waits = wait_for_wait_phases(
        &mut events,
        &["wait-input", "wait-exit"],
        /*started*/ false,
    )
    .await;
    let Some(ObservedWaitPhase::Finished(input_finished)) = finished_waits.get("wait-input") else {
        panic!("expected input wait to finish");
    };
    let Some(ObservedWaitPhase::Finished(exit_finished)) = finished_waits.get("wait-exit") else {
        panic!("expected exit wait to finish");
    };

    assert_eq!(
        *input_finished,
        TerminalWaitCompletionReason::Input,
        "new turn input should release only its own wait"
    );
    assert_eq!(
        *exit_finished,
        TerminalWaitCompletionReason::Exited,
        "the other process wait should continue until that process exits"
    );
    assert_eq!(input_response.process_id, Some(first_process_id));
    assert_eq!(exit_response.process_id, None);
    assert_eq!(exit_response.exit_code, Some(0));
    assert!(
        exit_response.wall_time >= Duration::from_secs(1),
        "wait_until_exit should ignore yield_time_ms and background_terminal_max_timeout"
    );

    let cancellation = CancellationToken::new();
    let cancellation_session = Arc::clone(&session);
    let cancellation_turn = Arc::clone(&turn);
    let cancellation_for_wait = cancellation.clone();
    let cancellation_wait = tokio::spawn(async move {
        tests::write_stdin_with_options(
            &cancellation_session,
            &cancellation_turn,
            first_process_id,
            "",
            /*yield_time_ms*/ 1,
            /*wait_until_exit*/ true,
            cancellation_for_wait,
            /*user_input_wait*/ None,
            Some("wait-cancel"),
        )
        .await
    });
    let cancellation_started =
        wait_for_wait_phases(&mut events, &["wait-cancel"], /*started*/ true).await;
    assert_eq!(
        cancellation_started.get("wait-cancel"),
        Some(&ObservedWaitPhase::Started)
    );
    cancellation.cancel();
    let cancellation_response = cancellation_wait
        .await
        .expect("cancelled wait task should finish")
        .expect("cancellation should return the still-live process state");
    let cancellation_finished =
        wait_for_wait_phases(&mut events, &["wait-cancel"], /*started*/ false).await;
    let Some(ObservedWaitPhase::Finished(cancellation_finished)) =
        cancellation_finished.get("wait-cancel")
    else {
        panic!("expected cancelled wait to finish");
    };

    let default_poll = tests::write_stdin_with_options(
        &session,
        &turn,
        first_process_id,
        "\n",
        /*yield_time_ms*/ 250,
        /*wait_until_exit*/ false,
        CancellationToken::new(),
        /*user_input_wait*/ None,
        /*interaction_id*/ None,
    )
    .await
    .expect("default bounded poll should complete");
    session
        .services
        .unified_exec_manager
        .terminate_all_processes()
        .await;

    assert_eq!(
        *cancellation_finished,
        TerminalWaitCompletionReason::Cancelled
    );
    assert_eq!(cancellation_response.process_id, Some(first_process_id));
    assert_eq!(
        default_poll.process_id,
        Some(first_process_id),
        "input release and cancellation must leave the background process resumable"
    );
    Ok(())
}

async fn wait_for_wait_phases(
    events: &mut async_channel::Receiver<Event>,
    interaction_ids: &[&str],
    started: bool,
) -> HashMap<String, ObservedWaitPhase> {
    let mut phases = HashMap::new();
    while phases.len() < interaction_ids.len() {
        let event = events
            .recv()
            .await
            .expect("session event stream should stay open");
        let EventMsg::TerminalInteraction(event) = event.msg else {
            continue;
        };
        let Some(wait) = event.wait else {
            continue;
        };
        match wait {
            TerminalWaitEvent::Started {
                interaction_id: event_interaction_id,
                ..
            } if started && interaction_ids.contains(&event_interaction_id.as_str()) => {
                phases.insert(event_interaction_id, ObservedWaitPhase::Started);
            }
            TerminalWaitEvent::Finished {
                interaction_id: event_interaction_id,
                reason,
                ..
            } if !started && interaction_ids.contains(&event_interaction_id.as_str()) => {
                phases.insert(event_interaction_id, ObservedWaitPhase::Finished(reason));
            }
            TerminalWaitEvent::Started { .. } | TerminalWaitEvent::Finished { .. } => {}
        }
    }
    phases
}
