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
            /*submission_id*/ 0,
            "first command".to_string(),
            cwd.clone(),
            Default::default(),
            first_cancel.clone(),
        ),
        manager.register_user_shell_command(
            "second".to_string(),
            /*submission_id*/ 1,
            "second command".to_string(),
            cwd.clone(),
            Default::default(),
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
            user_shell_response_handling: Some(Default::default()),
        },
        BackgroundTerminalInfo {
            item_id: "second".to_string(),
            process_id: second.to_string(),
            command: "second command".to_string(),
            cwd,
            user_shell_response_handling: Some(Default::default()),
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
            /*submission_id*/ 0,
            "running command".to_string(),
            cwd.clone(),
            Default::default(),
            running.clone(),
        )
        .await;
    manager.shutdown_user_shell_commands().await;
    assert!(running.is_cancelled());
    let late = CancellationToken::new();
    manager
        .register_user_shell_command(
            "late".to_string(),
            /*submission_id*/ 1,
            "late command".to_string(),
            cwd,
            Default::default(),
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
    manager
        .spawn_user_shell_command(async move {
            let token = CancellationToken::new();
            let id = worker_manager
                .register_user_shell_command(
                    "drained-command".to_string(),
                    /*submission_id*/ 0,
                    "pending command".to_string(),
                    PathUri::parse("file:///tmp").expect("cwd"),
                    Default::default(),
                    token.clone(),
                )
                .await;
            registered.send(id).expect("registration receipt");
            token.cancelled().await;
            cancelled.send(()).expect("cancellation receipt");
            released.await.expect("finish output publication");
            worker_manager
                .unregister_user_shell_command(id, "drained-command")
                .await;
        })
        .await
        .expect("admit producer");
    let id = registration.await.expect("registered producer");
    manager.shutdown_user_shell_commands().await;
    cancellation.await.expect("process cancellation requested");
    let mut drain = Box::pin(manager.drain_user_shell_commands());
    assert!(futures::poll!(drain.as_mut()).is_pending());
    assert_eq!(manager.list_processes().await[0].process_id, id.to_string());
    assert!(
        manager
            .spawn_user_shell_command(async { panic!("closed admission must not poll") })
            .await
            .is_err()
    );
    release.send(()).expect("allow publication to settle");
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), drain)
        .await
        .expect("producer drained");
    assert_eq!(manager.list_processes().await, Vec::new());
}

#[tokio::test]
async fn one_accepted_shell_producer_is_owned_by_both_shutdown_barriers() {
    let manager = std::sync::Arc::new(UnifiedExecProcessManager::default());
    let (started, running) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let published = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer_published = std::sync::Arc::clone(&published);
    manager
        .spawn_user_shell_command(async move {
            started.send(()).expect("producer started");
            released.await.expect("publication gate");
            producer_published.store(true, std::sync::atomic::Ordering::Release);
        })
        .await
        .expect("accept one producer");
    running.await.expect("producer is awaiting publication");
    manager.shutdown_user_shell_commands().await;
    let mut legacy_drain = Box::pin(manager.drain_user_shell_commands());
    let mut durable_drain = Box::pin(manager.shutdown_durably());
    assert!(futures::poll!(legacy_drain.as_mut()).is_pending());
    assert!(futures::poll!(durable_drain.as_mut()).is_pending());
    assert!(!published.load(std::sync::atomic::Ordering::Acquire));
    release
        .send(())
        .expect("allow canonical producer completion");
    let (_, durable) = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
        tokio::join!(legacy_drain, durable_drain)
    })
    .await
    .expect("both observers acknowledge the same producer");
    durable.expect("durable drain");
    assert!(published.load(std::sync::atomic::Ordering::Acquire));
}
