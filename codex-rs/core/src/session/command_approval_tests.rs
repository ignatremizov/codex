use std::sync::Arc;
use std::time::Duration;

use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::Submission;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio::time::advance;
use tokio::time::pause;
use tokio::time::timeout;

use super::super::SubmissionAdmission;
use super::CommandApprovalOrigin;
use super::PendingCommandApproval;
use super::QueuedSubmission;
use crate::state::TurnState;

fn submission(id: &str, op: Op) -> Submission {
    Submission {
        id: id.to_string(),
        op,
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
    }
}

fn approval_submission(id: &str) -> Submission {
    Submission {
        id: id.to_string(),
        op: Op::ExecApproval {
            id: "approval".to_string(),
            turn_id: Some("turn-1".to_string()),
            decision: ReviewDecision::Approved,
        },
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
    }
}

#[test]
fn claim_is_exclusive_and_released_only_by_its_submission() {
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, _waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );

    let claim = pending
        .claim("submission-1")
        .expect("first claim should succeed");
    assert!(pending.claim("submission-2").is_err());
    drop(claim);

    let replacement = pending
        .claim("submission-2")
        .expect("dropping the first claim should release its generation");
    drop(replacement);
}

#[tokio::test]
async fn dropping_the_waiter_invalidates_the_pending_request() {
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    drop(waiter);

    assert!(pending.claim("submission-1").is_err());
}

#[tokio::test]
async fn dropping_a_turn_owner_aborts_its_waiter() {
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (old_pending, waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let waiter = tokio::spawn(waiter.wait());
    drop(old_pending);
    assert_eq!(
        waiter.await.expect("waiter task should finish"),
        ReviewDecision::Abort
    );
}

#[tokio::test]
async fn clearing_turn_approvals_aborts_only_that_turns_waiter() {
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let waiter = tokio::spawn(waiter.wait());
    turn.lock()
        .await
        .command_approvals
        .insert("approval".to_string(), pending);
    turn.lock().await.command_approvals.clear();

    assert_eq!(
        waiter.await.expect("waiter task should finish"),
        ReviewDecision::Abort
    );
}

#[tokio::test]
async fn unclaimed_request_expires_without_becoming_a_claim() {
    pause();
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 5);
    let (pending, waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        Some(deadline),
    );
    let waiter = tokio::spawn(waiter.wait());

    advance(Duration::from_secs(/*secs*/ 5)).await;
    assert_eq!(
        waiter.await.expect("waiter task should finish"),
        ReviewDecision::TimedOut
    );
    assert!(pending.claim("submission-late").is_err());
}

#[tokio::test]
async fn a_claim_keeps_the_decision_authority_past_its_deadline() {
    pause();
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 5);
    let (pending, waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        Some(deadline),
    );
    let claim = pending
        .claim("submission-1")
        .expect("claim should succeed in time");
    let mut waiter = tokio::spawn(waiter.wait());

    advance(Duration::from_secs(/*secs*/ 5)).await;
    assert!(timeout(Duration::ZERO, &mut waiter).await.is_err());

    let sender = {
        let mut state = pending
            .0
            .state
            .lock()
            .expect("approval state should not be poisoned");
        assert!(matches!(state.phase, super::Phase::Claimed(ref id) if id == "submission-1"));
        state
            .sender
            .take()
            .expect("claimed approval should retain its sender")
    };
    sender
        .send(ReviewDecision::Approved)
        .expect("claimed approval should resolve after its deadline");
    assert_eq!(
        waiter.await.expect("waiter task should finish"),
        ReviewDecision::Approved
    );
    drop(claim);
    assert!(pending.claim("submission-late").is_err());
}

#[tokio::test]
async fn an_old_generation_cannot_release_a_replacement_cell() {
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (old_pending, _old_waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let old_claim = old_pending
        .claim("old-submission")
        .expect("old claim should succeed");
    let (new_pending, _new_waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    turn.lock()
        .await
        .command_approvals
        .insert("approval".to_string(), new_pending);

    drop(old_claim);
    let current = turn
        .lock()
        .await
        .command_approvals
        .get("approval")
        .expect("replacement approval should remain registered")
        .claim("new-submission")
        .expect("replacement generation should remain claimable");
    drop(current);
}

#[tokio::test]
async fn huge_timeout_is_safely_saturated_and_resolvable() {
    let now = Instant::now();
    let deadline = super::saturating_instant_add_ms(now, u64::MAX);
    assert!(deadline > now);
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        Some(deadline),
    );
    let claim = pending
        .claim("submission-huge")
        .expect("saturated deadline should remain claimable");
    let sender = {
        let mut state = pending
            .0
            .state
            .lock()
            .expect("approval state should not be poisoned");
        state
            .sender
            .take()
            .expect("approval should retain its sender")
    };
    sender
        .send(ReviewDecision::Approved)
        .expect("approval should resolve before the saturated deadline");
    assert_eq!(waiter.wait().await, ReviewDecision::Approved);
    drop(claim);
}

#[test]
fn full_queue_drops_only_the_failed_approval_envelope() {
    let (sender, receiver) = async_channel::bounded(/*cap*/ 1);
    sender
        .try_send(QueuedSubmission {
            submission: submission("before", Op::Compact),
            approval: None,
        })
        .expect("fill submission queue");
    let admission = Arc::new(SubmissionAdmission::default());
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, _waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let claim = pending.claim("rejected").expect("claim should succeed");

    let error = admission
        .try_enqueue(
            &sender,
            QueuedSubmission {
                submission: approval_submission("rejected"),
                approval: Some(claim),
            },
        )
        .expect_err("full queue should reject before acceptance");
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::InvalidRequest(_)
    ));
    assert!(pending.claim("retry").is_ok());
    assert_eq!(
        receiver
            .try_recv()
            .expect("original submission")
            .submission
            .id,
        "before"
    );
}

#[test]
fn closed_queue_drops_the_failed_approval_envelope() {
    let (sender, receiver) = async_channel::bounded(/*cap*/ 1);
    drop(receiver);
    let admission = Arc::new(SubmissionAdmission::default());
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, _waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let claim = pending.claim("rejected").expect("claim should succeed");

    let error = admission
        .try_enqueue(
            &sender,
            QueuedSubmission {
                submission: approval_submission("rejected"),
                approval: Some(claim),
            },
        )
        .expect_err("closed queue should reject before acceptance");
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::InternalAgentDied
    ));
    assert!(pending.claim("retry").is_ok());
}

#[tokio::test]
async fn cancelling_a_blocked_approval_envelope_releases_its_claim() {
    let (sender, receiver) = async_channel::bounded(/*cap*/ 1);
    sender
        .send(QueuedSubmission {
            submission: submission("before", Op::Compact),
            approval: None,
        })
        .await
        .expect("fill submission queue");
    let admission = Arc::new(SubmissionAdmission::default());
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, _waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let claim = pending.claim("blocked").expect("claim should succeed");
    let mut enqueue = Box::pin(admission.enqueue(
        &sender,
        QueuedSubmission {
            submission: approval_submission("blocked"),
            approval: Some(claim),
        },
    ));
    assert!(enqueue.as_mut().now_or_never().is_none());
    drop(enqueue);

    assert!(pending.claim("retry").is_ok());
    assert_eq!(
        receiver
            .recv()
            .await
            .expect("original submission")
            .submission
            .id,
        "before"
    );
}

#[test]
fn failed_envelope_does_not_clear_reload_quarantine() {
    let (sender, receiver) = async_channel::bounded(/*cap*/ 1);
    drop(receiver);
    let admission = Arc::new(SubmissionAdmission::default());
    admission.rollback_requires_reload();
    let turn = Arc::new(Mutex::new(TurnState::default()));
    let (pending, _waiter) = PendingCommandApproval::new(
        &turn,
        "turn-1".to_string(),
        CommandApprovalOrigin::RootCommand,
        /*deadline*/ None,
    );
    let claim = pending.claim("rejected").expect("claim should succeed");

    assert!(
        admission
            .try_enqueue(
                &sender,
                QueuedSubmission {
                    submission: approval_submission("rejected"),
                    approval: Some(claim),
                },
            )
            .is_err()
    );
    assert!(admission.requires_reload());
    assert!(pending.claim("retry").is_ok());
}
