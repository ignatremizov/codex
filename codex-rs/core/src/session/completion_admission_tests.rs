use super::*;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn submission(id: &str, op: Op) -> Submission {
    Submission {
        id: id.to_string(),
        op,
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
    }
}

#[tokio::test]
async fn completion_waits_through_correlated_rollback_publication() {
    let admission = Arc::new(SubmissionAdmission::default());
    let reservation = admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let (tx, rx) = async_channel::bounded(2);
    admission
        .try_enqueue(
            &tx,
            submission("rollback", Op::ThreadRollback { num_turns: 1 }),
        )
        .expect("rollback");
    assert_eq!(rx.recv().await.expect("queued").submission.id, "rollback");
    admission.begin_rollback_publication("rollback");
    admission.rollback_completed("rollback");
    admission
        .try_enqueue(&tx, submission("ordinary", Op::Compact))
        .expect("ordinary follow-up can enqueue");
    admission.finish_rollback_publication("older-rollback");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            admission.admit_completion(&reservation)
        )
        .await
        .is_err()
    );
    admission.finish_rollback_publication("rollback");
    let guard = tokio::time::timeout(
        Duration::from_secs(1),
        admission.admit_completion(&reservation),
    )
    .await
    .expect("released")
    .expect("healthy");
    drop(guard);
}

#[tokio::test]
async fn accepted_completion_crosses_shutdown_but_new_delivery_is_rejected() {
    let admission = Arc::new(SubmissionAdmission::default());
    let accepted = admission
        .try_accept_completion_delivery()
        .expect("accepted before shutdown");
    let (tx, _rx) = async_channel::bounded(1);
    admission
        .try_enqueue(&tx, submission("shutdown", Op::Shutdown))
        .expect("shutdown accepted");
    assert!(admission.try_accept_completion_delivery().is_none());
    assert!(
        admission
            .try_enqueue(&tx, submission("late-work", Op::Compact))
            .is_err()
    );
    drop(
        admission
            .admit_completion(&accepted)
            .await
            .expect("retained capability"),
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            admission.drain_accepted_completions()
        )
        .await
        .is_err()
    );
    drop(accepted);
    tokio::time::timeout(
        Duration::from_secs(1),
        admission.drain_accepted_completions(),
    )
    .await
    .expect("drained");
}

#[tokio::test]
async fn failed_shutdown_enqueue_reopens_only_its_own_admission() {
    let admission = Arc::new(SubmissionAdmission::default());
    let (tx, _rx) = async_channel::bounded(1);
    tx.try_send(submission("occupied", Op::Compact).into())
        .expect("full");
    assert!(
        admission
            .try_enqueue(&tx, submission("shutdown", Op::Shutdown))
            .is_err()
    );
    assert!(admission.try_accept_completion_delivery().is_some());
}

#[tokio::test]
async fn cancellation_cannot_unseal_another_accepted_shutdown() {
    let admission = Arc::new(SubmissionAdmission::default());
    let (tx, rx) = async_channel::bounded(1);
    tx.try_send(submission("occupied", Op::Compact).into())
        .expect("full");
    let mut first = Box::pin(admission.enqueue(&tx, submission("first", Op::Shutdown)));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut first)
            .await
            .is_err()
    );
    let mut second = Box::pin(admission.enqueue(&tx, submission("second", Op::Shutdown)));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    drop(first);
    rx.recv().await.expect("free slot");
    second.await.expect("second shutdown accepted");
    assert!(admission.try_accept_completion_delivery().is_none());
}

#[tokio::test]
async fn manager_removal_cannot_be_reopened_by_a_cancelled_shutdown_sender() {
    let admission = Arc::new(SubmissionAdmission::default());
    let accepted = admission
        .try_accept_completion_delivery()
        .expect("accepted before removal");
    let (tx, _rx) = async_channel::bounded(1);
    tx.try_send(submission("occupied", Op::Compact).into())
        .expect("full");
    let mut shutdown = Box::pin(admission.enqueue(&tx, submission("shutdown", Op::Shutdown)));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
            .await
            .is_err()
    );
    admission.close_completion_admission();
    drop(shutdown);
    assert!(admission.try_accept_completion_delivery().is_none());
    assert!(admission.admit_injection().await.is_err());
    drop(
        admission
            .admit_completion(&accepted)
            .await
            .expect("preaccepted exact-parent capability"),
    );
}

#[tokio::test]
async fn cancellation_of_blocked_shutdown_releases_admission_without_losing_rollback() {
    let admission = Arc::new(SubmissionAdmission::default());
    let (tx, _rx) = async_channel::bounded(1);
    tx.try_send(submission("occupied", Op::Compact).into())
        .expect("full");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            admission.enqueue(&tx, submission("shutdown", Op::Shutdown))
        )
        .await
        .is_err()
    );
    assert!(admission.try_accept_completion_delivery().is_some());
    assert!(admission.check_ready().is_ok());
}

#[tokio::test]
async fn quarantine_never_reopens_for_an_old_rollback_or_accepted_delivery() {
    let admission = Arc::new(SubmissionAdmission::default());
    let accepted = admission
        .try_accept_completion_delivery()
        .expect("accepted");
    admission.rollback_requires_reload();
    admission.rollback_completed("old");
    admission.finish_rollback_publication("old");
    assert!(admission.admit_completion(&accepted).await.is_err());
    assert!(admission.try_accept_completion_delivery().is_none());
    assert!(!admission.can_reload());
    admission.acknowledge_writer_closed();
    assert!(admission.can_reload());
}

#[tokio::test]
async fn accepted_capability_cannot_be_used_on_replacement_runtime() {
    let old = Arc::new(SubmissionAdmission::default());
    let replacement = SubmissionAdmission::default();
    let accepted = old
        .try_accept_completion_delivery()
        .expect("old generation");
    assert!(replacement.admit_completion(&accepted).await.is_err());
}
