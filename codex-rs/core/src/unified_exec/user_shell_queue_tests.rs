use std::sync::Arc;

use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::protocol::UserShellCommandResponseHandling;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecProcessManager;
use crate::unified_exec::UserShellCommandRetirement;

#[tokio::test]
async fn stopping_a_pending_submission_wins_before_its_launch_claim() {
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let first_submission = manager
        .reserve_user_shell_submission(UserShellCommandFinalDelivery::Passive)
        .await;
    let queued_submission = manager
        .reserve_user_shell_submission(UserShellCommandFinalDelivery::Passive)
        .await;
    let cancellation = CancellationToken::new();
    let process_id = manager
        .register_user_shell_command(
            "queued-call".to_string(),
            queued_submission,
            "queued command".to_string(),
            PathUri::parse("file:///tmp").expect("test cwd should be valid"),
            UserShellCommandResponseHandling::default(),
            cancellation.clone(),
        )
        .await;

    let waiter = {
        let manager = Arc::clone(&manager);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            manager
                .wait_for_user_shell_launch(
                    queued_submission,
                    /*queue_command*/ true,
                    &cancellation,
                )
                .await
        })
    };
    tokio::task::yield_now().await;

    assert!(manager.terminate_process(process_id).await);
    manager
        .release_user_shell_submission(first_submission)
        .await;
    assert!(!waiter.await.expect("launch waiter should join"));
    assert_eq!(
        manager
            .retire_user_shell_command(process_id, "queued-call")
            .await,
        Some(UserShellCommandRetirement::Stopped)
    );
}

#[tokio::test]
async fn stop_and_completion_retirement_have_one_atomic_winner() {
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let submission_id = manager
        .reserve_user_shell_submission(UserShellCommandFinalDelivery::Wake)
        .await;
    let cancellation = CancellationToken::new();
    let process_id = manager
        .register_user_shell_command(
            "wake-call".to_string(),
            submission_id,
            "wake command".to_string(),
            PathUri::parse("file:///tmp").expect("test cwd should be valid"),
            UserShellCommandResponseHandling {
                final_delivery: UserShellCommandFinalDelivery::Wake,
                queue_command: false,
            },
            cancellation.clone(),
        )
        .await;
    assert!(
        manager
            .wait_for_user_shell_launch(submission_id, /*queue_command*/ false, &cancellation,)
            .await
    );

    let retirement = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .retire_user_shell_command(process_id, "wake-call")
                .await
        })
    };
    let stop = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.terminate_process(process_id).await })
    };
    let (retirement, stopped) = tokio::join!(retirement, stop);
    let retirement = retirement.expect("retirement task should join");
    let stopped = stopped.expect("stop task should join");

    assert!(
        matches!(
            (retirement, stopped),
            (Some(UserShellCommandRetirement::Completed), false)
                | (Some(UserShellCommandRetirement::Stopped), true)
        ),
        "stop must either cancel delivery before retirement or report that completion retired first"
    );
    manager.release_user_shell_submission(submission_id).await;
}
