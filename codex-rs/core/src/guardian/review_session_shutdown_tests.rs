use super::*;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

#[tokio::test]
async fn closing_guardian_manager_fences_new_creation() {
    let manager = GuardianReviewSessionManager::default();
    manager
        .shutdown_durably()
        .await
        .expect("empty manager closes");
    let ran = Arc::new(AtomicBool::new(false));
    let execution = Arc::clone(&ran);
    let result = manager.state.lock().await.spawn_owned(async move {
        execution.store(true, Ordering::Release);
        Err(anyhow!("unexpected creation"))
    });
    assert!(result.is_err());
    assert!(!ran.load(Ordering::Acquire));
}

#[tokio::test]
async fn rejected_creation_is_not_a_phantom_owned_runtime() {
    let manager = GuardianReviewSessionManager::default();
    let creation = manager
        .state
        .lock()
        .await
        .spawn_owned(async { Err(anyhow!("configuration rejected before publication")) })
        .expect("accept creation");
    assert!(matches!(creation.await, Err(SpawnFailure::Rejected(_))));
    manager
        .shutdown_durably()
        .await
        .expect("no runtime to close");
    assert!(manager.state.lock().await.pending_spawns.is_empty());
}

#[tokio::test]
async fn lost_creation_task_cannot_be_treated_as_empty_manager() {
    let manager = GuardianReviewSessionManager::default();
    let creation = manager
        .state
        .lock()
        .await
        .spawn_owned(async { panic!("creation lost before reporting ownership") })
        .expect("accept creation");
    assert!(matches!(creation.await, Err(SpawnFailure::Lost(_))));
    assert!(manager.shutdown_durably().await.is_err());
    assert!(manager.shutdown_durably().await.is_err());
    assert_eq!(manager.state.lock().await.pending_spawns.len(), 1);
}

#[tokio::test]
async fn dropped_creation_waiter_does_not_release_the_creation_barrier() {
    let manager = GuardianReviewSessionManager::default();
    let (finish, completion) = oneshot::channel();
    let creation = manager
        .state
        .lock()
        .await
        .spawn_owned(async move {
            completion.await.expect("creation completion");
            Err(anyhow!("no runtime was created"))
        })
        .expect("accept creation");
    drop(creation);
    let mut shutdown = Box::pin(manager.shutdown_durably());
    assert!(shutdown.as_mut().now_or_never().is_none());
    // Shutdown does not hold the manager lock while the accepted creation settles.
    assert!(manager.state.try_lock().is_ok());
    finish.send(()).expect("settle accepted creation");
    shutdown.await.expect("creation settled without a runtime");
}
