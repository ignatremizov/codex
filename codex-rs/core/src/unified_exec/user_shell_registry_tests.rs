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
