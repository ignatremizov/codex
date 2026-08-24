use super::*;
use crate::codex_thread::BackgroundTerminalInfo;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn concurrent_shell_registration_and_exec_reservation_have_distinct_ids() {
    let manager = UnifiedExecProcessManager::default();
    let cwd = PathUri::parse("file:///tmp").expect("cwd");
    let first_cancel = CancellationToken::new();
    let second_cancel = CancellationToken::new();
    let (first, second, exec) = tokio::join!(
        manager.register_user_shell_command(
            "first".to_string(),
            "first command".to_string(),
            cwd.clone(),
            first_cancel.clone(),
        ),
        manager.register_user_shell_command(
            "second".to_string(),
            "second command".to_string(),
            cwd.clone(),
            second_cancel.clone(),
        ),
        manager.allocate_process_id(),
    );
    assert_ne!(first, second);
    assert_ne!(first, exec);
    assert_ne!(second, exec);
    let mut expected = vec![
        BackgroundTerminalInfo {
            item_id: "first".to_string(),
            process_id: first.to_string(),
            command: "first command".to_string(),
            cwd: cwd.clone(),
        },
        BackgroundTerminalInfo {
            item_id: "second".to_string(),
            process_id: second.to_string(),
            command: "second command".to_string(),
            cwd,
        },
    ];
    expected.sort_by_key(|entry| entry.process_id.parse::<i32>().expect("numeric process id"));
    assert_eq!(manager.list_processes().await, expected);

    manager
        .unregister_user_shell_command(first, "stale-call")
        .await;
    assert_eq!(manager.list_processes().await, expected);
    assert!(manager.terminate_process(first).await);
    assert!(first_cancel.is_cancelled());
    assert!(!second_cancel.is_cancelled());
    // Cancellation is not removal or proof of process exit.
    assert_eq!(manager.list_processes().await, expected);
    manager.unregister_user_shell_command(first, "first").await;
    manager.terminate_all_processes().await;
    assert!(second_cancel.is_cancelled());
    assert_eq!(manager.list_processes().await.len(), 1);
    manager
        .unregister_user_shell_command(second, "second")
        .await;
    assert_eq!(manager.list_processes().await, Vec::new());
}

#[tokio::test]
async fn shutdown_cancels_registered_and_late_preparing_shell_commands() {
    let manager = UnifiedExecProcessManager::default();
    let cwd = PathUri::parse("file:///tmp").expect("cwd");
    let running = CancellationToken::new();
    manager
        .register_user_shell_command(
            "running".to_string(),
            "running command".to_string(),
            cwd.clone(),
            running.clone(),
        )
        .await;
    manager.shutdown_user_shell_commands().await;
    assert!(running.is_cancelled());
    let late = CancellationToken::new();
    manager
        .register_user_shell_command(
            "late".to_string(),
            "late command".to_string(),
            cwd,
            late.clone(),
        )
        .await;
    assert!(late.is_cancelled());
}

#[tokio::test]
async fn shutdown_drain_waits_for_the_accepted_producer_not_just_its_cancel_request() {
    let manager = std::sync::Arc::new(UnifiedExecProcessManager::default());
    let (registered, registration) = tokio::sync::oneshot::channel();
    let (cancelled, cancellation) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let worker_manager = std::sync::Arc::clone(&manager);
    manager.spawn_user_shell_command(async move {
        let token = CancellationToken::new();
        let id = worker_manager.register_user_shell_command(
            "drained-command".to_string(), "pending command".to_string(),
            PathUri::parse("file:///tmp").expect("cwd"), token.clone(),
        ).await;
        registered.send(id).expect("registration receipt");
        token.cancelled().await;
        cancelled.send(()).expect("cancellation receipt");
        released.await.expect("finish output publication");
        worker_manager.unregister_user_shell_command(id, "drained-command").await;
    }).await.expect("admit producer");
    let id = registration.await.expect("registered producer");
    manager.shutdown_user_shell_commands().await;
    cancellation.await.expect("process cancellation requested");
    let mut drain = Box::pin(manager.drain_user_shell_commands());
    assert!(futures::poll!(drain.as_mut()).is_pending());
    assert_eq!(manager.list_processes().await[0].process_id, id.to_string());
    assert!(manager.spawn_user_shell_command(async { panic!("closed admission must not poll") }).await.is_err());
    release.send(()).expect("allow publication to settle");
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), drain)
        .await.expect("producer drained");
    assert_eq!(manager.list_processes().await, Vec::new());
}
