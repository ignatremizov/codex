use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalWaitCompletionReason;
use codex_protocol::protocol::TerminalWaitEvent;
use codex_protocol::protocol::TerminalWaitMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_stdin_waits_until_exit_and_reports_a_correlated_lifecycle() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX sleep command");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(test_codex()).await?;
    let test = harness.test();
    let requests = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_function_call(
                    "open-call",
                    "exec_command",
                    &json!({
                        "cmd": "sleep 12",
                        "yield_time_ms": 1,
                    })
                    .to_string(),
                ),
                ev_completed("open-response"),
            ]),
            sse(vec![
                ev_function_call(
                    "wait-call",
                    "write_stdin",
                    &json!({
                        "session_id": 1000,
                        "yield_time_ms": 1,
                        "wait_until_exit": true,
                    })
                    .to_string(),
                ),
                ev_completed("wait-response"),
            ]),
            sse(vec![
                ev_assistant_message("done-message", "The process exited."),
                ev_completed("done-response"),
            ]),
        ],
    )
    .await;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "start and wait for the command".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(test.default_environment_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                ..Default::default()
            }),
        )
        .await?;

    let mut wait_events = Vec::new();
    let mut command_ended = false;
    let mut turn_completed = false;
    while !command_ended || !turn_completed {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::TerminalInteraction(event) if event.call_id == "open-call" => {
                wait_events.push(event);
            }
            EventMsg::ExecCommandEnd(event) if event.call_id == "open-call" => {
                command_ended = true;
            }
            EventMsg::TurnComplete(_) => turn_completed = true,
            _ => {}
        }
    }

    assert_eq!(wait_events.len(), 2);
    let [started, finished] = wait_events.as_slice() else {
        panic!("expected a start and finish wait event: {wait_events:?}");
    };
    assert_eq!(started.process_id, "1000");
    assert_eq!(started.deadline_at_ms, None);
    assert!(matches!(
        &started.wait,
        Some(TerminalWaitEvent::Started {
            interaction_id,
            mode: TerminalWaitMode::UntilExit,
            ..
        }) if interaction_id == "wait-call"
    ));
    assert!(matches!(
        &finished.wait,
        Some(TerminalWaitEvent::Finished {
            interaction_id,
            elapsed_ms,
            reason: TerminalWaitCompletionReason::Exited,
        }) if interaction_id == "wait-call" && *elapsed_ms >= 6_000
    ));

    let wait_output = requests
        .requests()
        .iter()
        .find_map(|request| request.function_call_output_text("wait-call"))
        .expect("wait_stdin output should be present in a follow-up request");
    assert!(
        wait_output.contains("Process exited with code 0"),
        "wait_until_exit should outlast the short yield_time_ms and return the exit result: {wait_output}"
    );

    Ok(())
}
