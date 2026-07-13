use super::*;
use crate::session::tests::make_session_and_context;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Semaphore;

#[tokio::test]
async fn rejected_idle_admission_drops_lease_without_invoking_callback() {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let goal = Arc::new(Semaphore::new(/*permits*/ 1));
    let count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&count);
    let result = session
        .start_turn_if_idle_with_lease(
            TurnInputRequest::user_input(Vec::new()),
            goal.clone().acquire_owned().await.expect("goal lease"),
            move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .expect("admission decision");
    assert_eq!(
        result,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle
        }
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(goal.try_acquire().is_ok());
}

#[tokio::test]
async fn admitted_callback_runs_after_lease_release_before_task_registration() {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let goal = Arc::new(Semaphore::new(/*permits*/ 1));
    let lease = goal.clone().acquire_owned().await.expect("goal lease");
    let weak = Arc::downgrade(&session);
    let count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&count);
    let result = session
        .start_turn_if_idle_with_lease(TurnInputRequest::user_input(Vec::new()), lease, move |_| {
            assert!(goal.try_acquire().is_ok());
            let session = weak.upgrade().expect("admitting session");
            let active = session
                .active_turn
                .try_lock()
                .expect("admission holds no active-turn lock");
            assert!(
                active
                    .as_ref()
                    .expect("reserved placeholder")
                    .task
                    .is_none()
            );
            observed.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .expect("admission");
    assert!(matches!(result, TurnInputSubmission::Started { .. }));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}
