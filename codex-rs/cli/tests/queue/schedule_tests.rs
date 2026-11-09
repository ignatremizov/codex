use super::QueueResponse;
use super::THREAD_ID;
use super::respond_to_queue_request;
#[cfg(unix)]
use super::socket_test_home;
use anyhow::Context;
use anyhow::Result;
#[cfg(unix)]
use codex_login::AuthCredentialsStoreMode;
#[cfg(unix)]
use codex_login::AuthFileSelection;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;

#[tokio::test]
async fn schedule_wait_observes_its_receiving_turn_and_writes_the_final() -> Result<()> {
    let home = TempDir::new()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server_home = home.path().to_path_buf();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        respond_to_queue_request(stream, &server_home, QueueResponse::Schedule).await
    });
    let answer = home.path().join("answer.txt");
    let output = timeout(
        Duration::from_secs(/*secs*/ 30),
        Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
            .env("CODEX_HOME", home.path())
            .env_remove("CODEX_AUTH_FILE")
            .env("CODEX_SCHEDULE_REMOTE_TOKEN", "test-token")
            .current_dir(home.path())
            .kill_on_drop(true)
            .args([
                "schedule",
                "--remote",
                &endpoint,
                "--remote-auth-token-env",
                "CODEX_SCHEDULE_REMOTE_TOKEN",
                "--thread",
                THREAD_ID,
                "--at",
                "2026-11-09T17:30:00+02:00",
                "--message",
                "scheduled prompt",
                "--wait",
                "-o",
            ])
            .arg(&answer)
            .output(),
    )
    .await
    .context("schedule command timed out")??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = timeout(Duration::from_secs(/*secs*/ 10), server).await???;
    assert_eq!(
        (
            request["params"]["threadId"].as_str(),
            request["params"]["input"][0]["text"].as_str(),
            std::fs::read_to_string(answer)?
        ),
        (
            Some(THREAD_ID),
            Some("scheduled prompt"),
            "exact receiving turn answer".into()
        )
    );
    assert!(String::from_utf8(output.stdout)?.ends_with("exact receiving turn answer\n"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn exec_resume_reuses_the_profile_daemon_and_emits_standard_final_json() -> Result<()> {
    let home = socket_test_home()?;
    let profile = AuthFileSelection::Default
        .profile_identity(home.path(), AuthCredentialsStoreMode::default());
    let socket =
        codex_app_server::app_server_profile_socket_path(home.path(), &profile.profile_opaque_id)?;
    std::fs::create_dir_all(socket.as_path().parent().context("socket parent")?)?;
    let listener = tokio::net::UnixListener::bind(socket.as_path())?;
    let server_home = home.path().to_path_buf();
    let server = tokio::spawn(async move {
        let (probe, _) = listener.accept().await?;
        drop(probe);
        let (stream, _) = listener.accept().await?;
        respond_to_queue_request(stream, &server_home, QueueResponse::Schedule).await
    });
    let output = timeout(
        Duration::from_secs(/*secs*/ 30),
        Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
            .env("CODEX_HOME", home.path())
            .env_remove("CODEX_AUTH_FILE")
            .current_dir(home.path())
            .kill_on_drop(true)
            .args(["exec", "--json", "resume", THREAD_ID, "resume prompt"])
            .output(),
    )
    .await
    .context("shared exec command timed out")??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = timeout(Duration::from_secs(/*secs*/ 10), server).await???;
    assert_eq!(request["params"]["input"][0]["text"], "resume prompt");
    let events = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(
        events,
        vec![
            serde_json::json!({"type": "thread.started", "thread_id": THREAD_ID}),
            serde_json::json!({"type": "turn.started"}),
            serde_json::json!({
                "type": "item.completed",
                "item": {"id": "actual-final", "type": "agent_message", "text": "exact receiving turn answer"},
            }),
            serde_json::json!({
                "type": "turn.completed",
                "usage": {
                    "input_tokens": 0,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                },
            }),
        ]
    );
    Ok(())
}
