//! A queue reservation is owned before a detached worker can first be polled.

use crate::session::tests::make_session_and_context_with_rx;
use crate::tasks::UserShellCommandPlacement;
use crate::tasks::execute_user_shell_command;
use codex_protocol::protocol::UserShellCommandFinalDelivery;
use codex_protocol::protocol::UserShellCommandResponseHandling;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test_case::test_case(false; "never polled execution")]
#[test_case::test_case(true; "scheduling rejected after shutdown")]
#[tokio::test]
async fn unstarted_execution_releases_its_queue_reservation(reject_scheduling: bool) {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    if reject_scheduling {
        manager.shutdown_user_shell_commands().await;
    }
    let submission = manager
        .reserve_user_shell_submission(UserShellCommandFinalDelivery::Wake)
        .await;
    let execution = execute_user_shell_command(
        Arc::clone(&session),
        turn,
        "must not execute".to_string(),
        /*timeout_ms*/ None,
        UserShellCommandPlacement::Detached,
        UserShellCommandResponseHandling {
            final_delivery: UserShellCommandFinalDelivery::Wake,
            queue_command: true,
        },
        submission,
        session
            .submission_admission
            .try_accept_completion_delivery()
            .expect("shell accepted before scheduling"),
    );
    assert!(manager.has_pending_user_shell_completion_wake().await);
    if reject_scheduling {
        assert!(manager.spawn_user_shell_command(execution).await.is_err());
    } else {
        drop(execution);
    }
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while manager.has_pending_user_shell_completion_wake().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("abandoned submission releases its pending wake");
    assert!(manager.list_processes().await.is_empty());
    assert!(
        session
            .clone_history()
            .await
            .into_annotated_items()
            .is_empty()
    );
    assert!(events.try_recv().is_err());
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        session.submission_admission.drain_accepted_completions(),
    )
    .await
    .expect("abandoned execution releases its exact-session completion receipt");
    if !reject_scheduling {
        let next = manager
            .reserve_user_shell_submission(UserShellCommandFinalDelivery::Passive)
            .await;
        assert!(
            tokio::time::timeout(
                Duration::from_secs(/*secs*/ 5),
                manager.wait_for_user_shell_launch(
                    next,
                    /*queue_command*/ true,
                    &CancellationToken::new(),
                ),
            )
            .await
            .expect("a later queued launch is not blocked by the abandoned reservation")
        );
        manager.release_user_shell_submission(next).await;
    }
}
