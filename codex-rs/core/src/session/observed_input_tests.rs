use super::*;
use crate::session::command_approval::QueuedSubmission;
use crate::session::session_loop_termination_from_handle;
use crate::session::tests::make_session_and_context;
use codex_protocol::protocol::AgentStatus;
use futures::poll;
use pretty_assertions::assert_eq;
use std::sync::Arc;

async fn fixture() -> (
    Arc<Session>,
    SessionIo,
    async_channel::Receiver<QueuedSubmission>,
) {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    let (_, rx_event) = async_channel::unbounded();
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(&session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: tokio::sync::watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(tokio::spawn(async {})),
    };
    (session, io, rx_sub)
}

#[tokio::test]
async fn idle_rejection_is_not_an_admitted_or_uncertain_input() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartIfIdle,
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    let Op::TurnInput { mode, reply, .. } = queued.op else {
        panic!("turn input")
    };
    assert_eq!(mode, TurnInputMode::StartIfIdle);
    reply
        .send(Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle,
        }))
        .unwrap();
    assert!(matches!(
        submission.await.unwrap(),
        ObservedTurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle
        }
    ));
    assert!(!session.submission_admission.requires_reload());
}

#[tokio::test]
async fn accepted_input_keeps_actual_turn_when_observation_receipt_fails() {
    for routing in [
        TurnInputSubmission::Started {
            turn_id: "actual-turn".to_string(),
        },
        TurnInputSubmission::Steered {
            turn_id: "actual-turn".to_string(),
        },
    ] {
        let (session, io, submissions) = fixture().await;
        let mut submission = Box::pin(io.submit_observed_turn_input(
            &session,
            TurnInputRequest::user_input(Vec::new()),
            TurnInputMode::StartOrSteer,
        ));
        assert!(poll!(&mut submission).is_pending());
        let queued = submissions.recv().await.unwrap().submission;
        let Op::TurnInput { reply, .. } = queued.op else {
            panic!("turn input")
        };
        reply.send(Ok(routing)).unwrap();
        session.reject_input_turn_admission(&queued.id, CodexErr::InternalAgentDied);
        let ObservedTurnInputSubmission::AdmittedWithoutObservation {
            submission_id,
            target_turn_id,
            warning,
        } = submission.await.unwrap()
        else {
            panic!("accepted with warning")
        };
        assert_eq!(
            (submission_id, target_turn_id),
            (queued.id, "actual-turn".to_string())
        );
        assert!(warning.contains("do not resubmit"));
        assert!(session.submission_admission.requires_reload());
    }
}

#[tokio::test]
async fn routing_loss_after_enqueue_is_not_proof_of_non_admission() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartOrSteer,
    ));
    assert!(poll!(&mut submission).is_pending());
    let queued = submissions.recv().await.unwrap().submission;
    let id = queued.id.clone();
    drop(queued);
    let ObservedTurnInputSubmission::Indeterminate {
        submission_id,
        warning,
    } = submission.await.unwrap()
    else {
        panic!("unknown routing")
    };
    assert_eq!(submission_id, id);
    assert!(warning.contains("do not resubmit"));
    assert!(session.submission_admission.requires_reload());
}

#[tokio::test]
async fn cancelled_waiter_cannot_change_queued_idle_only_mode() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_observed_turn_input(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartIfIdle,
    ));
    assert!(poll!(&mut submission).is_pending());
    drop(submission);
    let queued = submissions.recv().await.unwrap().submission;
    let Op::TurnInput { mode, .. } = queued.op else {
        panic!("turn input")
    };
    assert_eq!(mode, TurnInputMode::StartIfIdle);
}

#[tokio::test]
async fn strict_adapter_rejects_uncertain_input_and_quarantines() {
    let (session, io, submissions) = fixture().await;
    let mut submission = Box::pin(io.submit_turn_input_with_admission(
        &session,
        TurnInputRequest::user_input(Vec::new()),
        TurnInputMode::StartOrSteer,
    ));
    assert!(poll!(&mut submission).is_pending());
    drop(submissions.recv().await.unwrap());
    assert!(submission.await.is_err());
    assert!(session.submission_admission.requires_reload());
}
