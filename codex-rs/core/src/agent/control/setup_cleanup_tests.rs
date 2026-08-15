use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

#[tokio::test]
async fn cancelled_explicit_rollback_finishes_cleanup_in_background() {
    let cleanup_started = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let cleanup_finished = Arc::new(AtomicBool::new(false));
    let guard = SetupCleanupGuard::new("test explicit rollback", {
        let cleanup_started = Arc::clone(&cleanup_started);
        let release_cleanup = Arc::clone(&release_cleanup);
        let cleanup_finished = Arc::clone(&cleanup_finished);
        async move {
            cleanup_started.notify_one();
            release_cleanup.notified().await;
            cleanup_finished.store(true, Ordering::Release);
            Ok(())
        }
    });
    let rollback = tokio::spawn(guard.rollback());

    timeout(Duration::from_secs(/*secs*/ 1), cleanup_started.notified())
        .await
        .expect("explicit rollback should start cleanup");
    rollback.abort();
    assert!(
        rollback
            .await
            .expect_err("rollback task should be cancelled")
            .is_cancelled()
    );

    release_cleanup.notify_one();
    timeout(Duration::from_secs(/*secs*/ 1), async {
        while !cleanup_finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled explicit rollback should finish cleanup in the background");
}
