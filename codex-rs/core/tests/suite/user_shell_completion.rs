use anyhow::Context;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::protocol::UserShellCommandResponseHandling;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_shell_command_wake_starts_an_idle_turn_with_the_result() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    super::user_shell_processes::select_local_user_shell_environment(&test).await?;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-shell-wake"),
            ev_assistant_message("msg-shell-wake", "wake observed"),
            ev_completed("resp-shell-wake"),
        ]),
    )
    .await;

    #[cfg(windows)]
    let command = "Write-Output shell-wake-result".to_string();
    #[cfg(not(windows))]
    let command = "printf shell-wake-result".to_string();
    test.codex
        .submit(Op::RunUserShellCommand {
            command,
            timeout_ms: None,
            response_handling: UserShellCommandResponseHandling {
                final_delivery: UserShellCommandFinalDelivery::Wake,
                queue_command: false,
            },
        })
        .await?;

    let end = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(end.exit_code, 0);
    let completed = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnComplete(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        completed.last_agent_message.as_deref(),
        Some("wake observed")
    );

    let request = mock.single_request();
    let turn_metadata: serde_json::Value = serde_json::from_str(
        request
            .header("x-codex-turn-metadata")
            .as_deref()
            .expect("wake request should include turn metadata"),
    )?;
    assert_eq!(
        turn_metadata["turn_trigger"].as_str(),
        Some("user_shell_wake")
    );
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("<user_shell_command>")
                && text.contains("shell-wake-result")),
        "wake request should contain the completed user-shell result"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presentation_only_user_shell_result_stays_out_of_model_context() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    super::user_shell_processes::select_local_user_shell_environment(&test).await?;

    #[cfg(windows)]
    let command = "Write-Output private-shell-result".to_string();
    #[cfg(not(windows))]
    let command = "printf private-shell-result".to_string();
    test.codex
        .submit(Op::RunUserShellCommand {
            command,
            timeout_ms: None,
            response_handling: UserShellCommandResponseHandling {
                final_delivery: UserShellCommandFinalDelivery::PresentationOnly,
                queue_command: false,
            },
        })
        .await?;
    let _ = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => Some(()),
        _ => None,
    })
    .await;

    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-after-private-shell"),
            ev_assistant_message("msg-after-private-shell", "done"),
            ev_completed("resp-after-private-shell"),
        ]),
    )
    .await;
    test.submit_turn("continue after private shell").await?;

    assert!(
        mock.single_request()
            .message_input_texts("user")
            .iter()
            .all(|text| !text.contains("private-shell-result")),
        "presentation-only shell output must not enter model context"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_user_shell_command_waits_for_earlier_submissions() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    super::user_shell_processes::select_local_user_shell_environment(&test).await?;
    let temp = TempDir::new()?;
    let started_path = temp.path().join("first-started");
    let release_path = temp.path().join("release-first");
    let second_started_path = temp.path().join("second-started");
    let cancelled_started_path = temp.path().join("cancelled-started");
    let order_path = temp.path().join("order");

    #[cfg(windows)]
    let (first_command, second_command, cancelled_command) = {
        let quote = |path: &std::path::Path| path.display().to_string().replace('\'', "''");
        (
            format!(
                "Set-Content -LiteralPath '{}' -Value started; while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 10 }}; Add-Content -LiteralPath '{}' -Value first",
                quote(&started_path),
                quote(&release_path),
                quote(&order_path),
            ),
            format!(
                "Set-Content -LiteralPath '{}' -Value started; Add-Content -LiteralPath '{}' -Value second",
                quote(&second_started_path),
                quote(&order_path),
            ),
            format!(
                "Set-Content -LiteralPath '{}' -Value started",
                quote(&cancelled_started_path),
            ),
        )
    };
    #[cfg(not(windows))]
    let (first_command, second_command, cancelled_command) = {
        let quote = |path: &std::path::Path| {
            format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
        };
        (
            format!(
                "printf started > {}; while [ ! -f {} ]; do sleep 0.01; done; printf 'first\\n' >> {}",
                quote(&started_path),
                quote(&release_path),
                quote(&order_path),
            ),
            format!(
                "printf started > {}; printf 'second\\n' >> {}",
                quote(&second_started_path),
                quote(&order_path),
            ),
            format!("printf started > {}", quote(&cancelled_started_path)),
        )
    };

    test.codex
        .submit(Op::RunUserShellCommand {
            command: first_command,
            timeout_ms: None,
            response_handling: Default::default(),
        })
        .await?;
    let _ = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event) if event.source == ExecCommandSource::UserShell => {
            Some(())
        }
        _ => None,
    })
    .await;
    timeout(Duration::from_secs(5), async {
        while !started_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("first user-shell command did not start")?;

    test.codex
        .submit(Op::RunUserShellCommand {
            command: second_command,
            timeout_ms: None,
            response_handling: UserShellCommandResponseHandling {
                final_delivery: UserShellCommandFinalDelivery::Passive,
                queue_command: true,
            },
        })
        .await?;
    let _ = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event) if event.source == ExecCommandSource::UserShell => {
            Some(())
        }
        _ => None,
    })
    .await;

    test.codex
        .submit(Op::RunUserShellCommand {
            command: cancelled_command,
            timeout_ms: None,
            response_handling: UserShellCommandResponseHandling {
                final_delivery: UserShellCommandFinalDelivery::Passive,
                queue_command: true,
            },
        })
        .await?;
    let cancelled_begin = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        test.codex.list_background_terminals().await.len(),
        3,
        "queued user-shell command should remain discoverable before launch"
    );
    let cancelled_process_id = cancelled_begin
        .process_id
        .as_deref()
        .context("queued user-shell command should expose a process id")?
        .parse::<i32>()
        .context("queued user-shell process id should be numeric")?;
    assert!(
        test.codex
            .terminate_background_terminal(cancelled_process_id)
            .await
    );
    let _ = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event)
            if event.source == ExecCommandSource::UserShell
                && event.call_id == cancelled_begin.call_id =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(
        !cancelled_started_path.exists(),
        "stopped queued command started before it was cancelled"
    );
    assert!(
        timeout(Duration::from_millis(200), async {
            while !second_started_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_err(),
        "queued command started before the earlier user-shell command finished"
    );

    tokio::fs::write(&release_path, "release").await?;
    for _ in 0..2 {
        let _ = wait_for_event_match(&test.codex, |event| match event {
            EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
                Some(())
            }
            _ => None,
        })
        .await;
    }
    let order = tokio::fs::read_to_string(order_path).await?;
    assert_eq!(order.replace("\r\n", "\n"), "first\nsecond\n");

    Ok(())
}
