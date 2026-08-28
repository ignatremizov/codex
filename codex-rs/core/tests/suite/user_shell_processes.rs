use anyhow::Context;
use codex_core::BackgroundTerminalInfo;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::protocol::UserShellCommandResponseHandling;
use codex_protocol::user_input::UserInput;
use core_test_support::PathExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_with_delay;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::timeout;

pub(super) async fn select_local_user_shell_environment(test: &TestCodex) -> anyhow::Result<()> {
    let local_cwd = test.cwd_path().abs();
    let mut environments = vec![local(local_cwd.clone())];
    if test.executor_environment().environment().is_remote() {
        environments.insert(
            /*index*/ 0,
            test.executor_environment().selection().clone(),
        );
    }
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            environments: Some(TurnEnvironmentSelections::new(local_cwd, environments)),
            ..Default::default()
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_model_interruption_does_not_cancel_user_shell() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    select_local_user_shell_environment(&test).await?;
    let request = mount_sse_once_with_delay(
        &server,
        sse(vec![ev_response_created("slow"), ev_completed("slow")]),
        Duration::from_secs(/*secs*/ 60),
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "wait for a response".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    timeout(Duration::from_secs(/*secs*/ 5), async {
        while request.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    #[cfg(windows)]
    let command = "Write-Output shell-running; Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let command = "printf 'shell-running\\n'; exec sleep 60";
    test.codex
        .submit(Op::RunUserShellCommand {
            command: command.to_string(),
            timeout_ms: None,
            response_handling: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandOutputDelta(delta)
            if String::from_utf8_lossy(&delta.chunk).contains("shell-running") =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    let before = test.codex.list_background_terminals().await;
    assert_eq!(before.len(), 1);
    test.codex.submit(Op::Interrupt).await?;
    let aborted = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnAborted(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(aborted.reason, TurnAbortReason::Interrupted);
    assert!(
        timeout(
            Duration::from_millis(/*millis*/ 250),
            wait_for_event_match(&test.codex, |event| match event {
                EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell =>
                    Some(()),
                _ => None,
            }),
        )
        .await
        .is_err()
    );
    assert_eq!(test.codex.list_background_terminals().await, before);
    assert!(
        test.codex
            .terminate_background_terminal(before[0].process_id.parse()?)
            .await
    );
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => Some(()),
        _ => None,
    })
    .await;
    Ok(())
}

#[tokio::test]
async fn default_user_shell_outlives_old_deadline_and_model_interrupt() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    select_local_user_shell_environment(&test).await?;
    #[cfg(windows)]
    let command = "Write-Output shell-running; Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let command = "printf 'shell-running\\n'; exec sleep 60";
    test.codex
        .submit(Op::RunUserShellCommand {
            command: command.to_string(),
            timeout_ms: None,
            response_handling: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandOutputDelta(delta)
            if String::from_utf8_lossy(&delta.chunk).contains("shell-running") =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    let before = test.codex.list_background_terminals().await;
    assert_eq!(before.len(), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(/*secs*/ 3_601)).await;
    tokio::time::resume();
    test.codex.submit(Op::Interrupt).await?;
    assert!(
        timeout(
            Duration::from_millis(/*millis*/ 250),
            wait_for_event_match(&test.codex, |event| match event {
                EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
                    Some(())
                }
                _ => None,
            }),
        )
        .await
        .is_err(),
        "default shell deadline or model interruption ended the detached process"
    );
    assert_eq!(test.codex.list_background_terminals().await, before);
    test.codex.submit(Op::CleanBackgroundTerminals).await?;
    let end = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        end.process_id.as_deref(),
        Some(before[0].process_id.as_str())
    );
    assert_ne!(end.exit_code, 0);
    Ok(())
}

#[tokio::test]
async fn explicit_positive_timeout_overrides_configured_limit() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        let user_config = config.codex_home.join(codex_config::CONFIG_TOML_FILE);
        config.config_layer_stack = config
            .config_layer_stack
            .with_user_config(
                &user_config,
                toml::toml! { user_shell_command_timeout_ms = 60_000 }.into(),
            )
            .expect("configured deadline");
    });
    let test = builder.build_with_remote_and_local_env(&server).await?;
    select_local_user_shell_environment(&test).await?;
    #[cfg(windows)]
    let command = "Write-Output shell-running; Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let command = "printf 'shell-running\\n'; exec sleep 60";
    test.codex
        .submit(Op::RunUserShellCommand {
            command: command.to_string(),
            timeout_ms: Some(120_000),
            response_handling: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandOutputDelta(delta)
            if String::from_utf8_lossy(&delta.chunk).contains("shell-running") =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    let completed = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    });
    tokio::pin!(completed);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(/*secs*/ 61)).await;
    tokio::time::resume();
    assert!(
        timeout(Duration::from_millis(/*millis*/ 250), &mut completed)
            .await
            .is_err()
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(/*secs*/ 60)).await;
    tokio::time::resume();
    let end = timeout(Duration::from_secs(/*secs*/ 10), &mut completed).await?;
    assert_eq!(end.exit_code, 124);
    assert!(end.aggregated_output.contains("shell-running"));
    Ok(())
}

#[tokio::test]
async fn explicit_zero_user_shell_timeout_is_immediate() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    select_local_user_shell_environment(&test).await?;
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let command = "sleep 60";
    test.codex
        .submit(Op::RunUserShellCommand {
            command: command.to_string(),
            timeout_ms: Some(0),
            response_handling: Default::default(),
        })
        .await?;
    let end = timeout(
        Duration::from_secs(/*secs*/ 10),
        wait_for_event_match(&test.codex, |event| match event {
            EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
                Some(event.clone())
            }
            _ => None,
        }),
    )
    .await?;
    assert_eq!(end.exit_code, 124);
    assert!(end.formatted_output.contains("command timed out"));
    Ok(())
}

#[tokio::test]
async fn user_shell_wake_is_listed_and_targeted_stop_suppresses_its_wake() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    select_local_user_shell_environment(&test).await?;
    let unexpected_wake = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("unexpected-shell-wake"),
            ev_completed("unexpected-shell-wake"),
        ]),
    )
    .await;
    let response_handling = UserShellCommandResponseHandling {
        final_delivery: UserShellCommandFinalDelivery::Wake,
        queue_command: false,
    };

    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 60".to_string();
    #[cfg(not(windows))]
    let command = "sleep 60".to_string();

    test.codex
        .submit(Op::RunUserShellCommand {
            command: command.clone(),
            timeout_ms: None,
            response_handling,
        })
        .await?;

    let begin = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    let process_id = begin
        .process_id
        .clone()
        .context("user shell command should expose a process id")?;
    assert_eq!(
        test.codex.list_background_terminals().await,
        vec![BackgroundTerminalInfo {
            item_id: begin.call_id.clone(),
            process_id: process_id.clone(),
            command,
            cwd: begin.cwd,
            user_shell_response_handling: Some(response_handling),
        }]
    );

    let numeric_process_id = process_id
        .parse::<i32>()
        .context("user shell process id should be numeric")?;
    assert!(
        test.codex
            .terminate_background_terminal(numeric_process_id)
            .await
    );

    let end = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(end.process_id, Some(process_id));

    timeout(Duration::from_secs(3), async {
        loop {
            if test.codex.list_background_terminals().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("stopped user shell command remained in background terminal list")?;
    assert!(
        timeout(Duration::from_millis(200), async {
            while unexpected_wake.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_err(),
        "targeted stop must suppress a user-shell completion wake"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_user_shell_command_does_not_block_a_model_turn() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    select_local_user_shell_environment(&test).await?;

    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 60".to_string();
    #[cfg(not(windows))]
    let command = "sleep 60".to_string();

    test.codex
        .submit(Op::RunUserShellCommand {
            command: command.clone(),
            timeout_ms: None,
            response_handling: Default::default(),
        })
        .await?;
    let begin = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event) if event.source == ExecCommandSource::UserShell => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    let process_id = begin
        .process_id
        .clone()
        .context("user shell command should expose a process id")?;

    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "model turn completed"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    test.submit_turn("continue while the user shell command runs")
        .await?;

    timeout(Duration::from_secs(5), async {
        loop {
            if mock.requests().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("idle user shell command blocked the model turn")?;
    assert_eq!(
        test.codex.list_background_terminals().await,
        vec![BackgroundTerminalInfo {
            item_id: begin.call_id.clone(),
            process_id: process_id.clone(),
            command,
            cwd: begin.cwd.clone(),
            user_shell_response_handling: Some(Default::default()),
        }]
    );

    let process_id = process_id
        .parse::<i32>()
        .context("user shell process id should be numeric")?;
    assert!(test.codex.terminate_background_terminal(process_id).await);
    let _ = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandEnd(event) if event.source == ExecCommandSource::UserShell => Some(()),
        _ => None,
    })
    .await;

    Ok(())
}
