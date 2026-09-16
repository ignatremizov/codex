use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::SessionCleanup;

#[tokio::test]
async fn failed_connection_requires_actual_host_exit_and_callback_completion() {
    let exited = CancellationToken::new();
    let cleanup = SessionCleanup::new(exited.clone());
    let callback = cleanup.accepted_callback();
    cleanup.fail(Vec::new());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), cleanup.wait_durably())
            .await
            .is_err()
    );
    exited.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), cleanup.wait_durably())
            .await
            .is_err()
    );
    drop(callback);
    assert_eq!(cleanup.wait_durably().await, Ok(()));
}

#[tokio::test]
async fn callback_panic_is_not_mistaken_for_successful_drain() {
    let cleanup = SessionCleanup::new(CancellationToken::new());
    let callback = cleanup.accepted_callback();
    let task = tokio::spawn(async move {
        let _callback = callback;
        panic!("delegate panic");
    });
    assert!(task.await.is_err());
    cleanup.close();
    assert_eq!(
        cleanup.wait_durably().await,
        Err("accepted code-mode provider callback panicked".to_string())
    );
}
