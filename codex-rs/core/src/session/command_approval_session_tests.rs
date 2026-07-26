use std::sync::Arc;
use std::time::Duration;

use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use tokio::time::Instant;

use super::CommandApprovalOrigin;
use super::CommandApprovalWaiter;
use super::PendingCommandApproval;
use super::Session;
use crate::session::approval_test_support::start_approval_turn;
use crate::session::handlers;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;

#[derive(Clone, Copy)]
enum SubmitMode {
    Ordinary,
    Nonblocking,
    Traced,
    ExplicitId,
    Forwarded,
}

async fn register(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    deadline: Option<Instant>,
) -> CommandApprovalWaiter {
    let origin = Arc::clone(
        &session
            .active_turn
            .lock()
            .await
            .as_ref()
            .expect("active turn")
            .turn_state,
    );
    let (pending, waiter) = PendingCommandApproval::new(
        &origin,
        turn.sub_id.clone(),
        CommandApprovalOrigin::RootCommand,
        deadline,
    );
    origin
        .lock()
        .await
        .command_approvals
        .insert("approval".into(), pending);
    waiter
}

fn response(turn: &TurnContext, submission_id: &str, decision: ReviewDecision) -> Submission {
    Submission {
        id: submission_id.into(),
        op: Op::ExecApproval {
            id: "approval".into(),
            turn_id: Some(turn.sub_id.clone()),
            decision,
        },
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
    }
}

#[tokio::test]
async fn every_submission_path_transfers_one_claim_to_the_ordered_queue() {
    for mode in [
        SubmitMode::Ordinary,
        SubmitMode::Nonblocking,
        SubmitMode::Traced,
        SubmitMode::ExplicitId,
        SubmitMode::Forwarded,
    ] {
        let (session, turn, events) = make_session_and_context_with_rx().await;
        start_approval_turn(&session, &turn, &events).await;
        let waiter = register(&session, &turn, /*deadline*/ None).await;
        let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 4);
        let (_tx_event, rx_event) = async_channel::unbounded();
        let io = Arc::new(crate::session::SessionIo {
            tx_sub,
            session: Arc::downgrade(&session),
            rx_event,
            submission_admission: Arc::clone(&session.submission_admission),
            agent_status: tokio::sync::watch::channel(
                codex_protocol::protocol::AgentStatus::PendingInit,
            )
            .1,
            session_loop_termination: crate::session::completed_session_loop_termination(),
        });
        let submission = response(&turn, "explicit", ReviewDecision::Approved);
        let id = match mode {
            SubmitMode::Ordinary => io.submit(submission.op).await.unwrap(),
            SubmitMode::Nonblocking => io.try_submit(submission.op).unwrap(),
            SubmitMode::Traced => io
                .submit_with_trace(
                    submission.op,
                    /*trace*/ None,
                    Some("parent".into()),
                    Some("root".into()),
                )
                .await
                .unwrap(),
            SubmitMode::ExplicitId => {
                io.submit_with_id(submission).await.unwrap();
                "explicit".into()
            }
            SubmitMode::Forwarded => crate::codex_delegate::forward_session_io(
                Arc::clone(&io),
                tokio_util::sync::CancellationToken::new(),
            )
            .submit(submission.op)
            .await
            .unwrap(),
        };
        assert!(
            io.try_submit(response(&turn, "duplicate", ReviewDecision::Approved).op)
                .is_err()
        );
        let queued = rx_sub.recv().await.expect("accepted approval");
        assert_eq!(queued.submission.id, id);
        if matches!(mode, SubmitMode::Traced) {
            assert_eq!(
                (
                    queued.submission.parent_turn_id,
                    queued.submission.root_turn_id
                ),
                (Some("parent".into()), Some("root".into()))
            );
        }
        handlers::exec_approval(
            &session,
            "approval".into(),
            Some(turn.sub_id.clone()),
            ReviewDecision::Approved,
            &id,
            queued.approval,
        )
        .await;
        assert_eq!(waiter.wait().await, ReviewDecision::Approved);
        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }
}

#[tokio::test]
async fn timely_claim_is_consumed_after_deadline_under_origin_exclusion() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    start_approval_turn(&session, &turn, &events).await;
    tokio::time::pause();
    let waiter = register(
        &session,
        &turn,
        Some(Instant::now() + Duration::from_secs(/*secs*/ 5)),
    )
    .await;
    let submission = response(&turn, "timely", ReviewDecision::Approved);
    let claim = session
        .claim_command_approval(&submission)
        .await
        .unwrap()
        .unwrap();
    assert!(session.claim_command_approval(&submission).await.is_err());
    tokio::time::advance(Duration::from_secs(/*secs*/ 5)).await;
    let accepted = session
        .consume_command_approval("approval", "timely", claim)
        .await
        .expect("timely authority survives ordered queue delay");
    assert!(session.active_turn.try_lock().is_err());
    // Policy persistence in the handler uses this same retained exclusion.
    drop(accepted.active_guard);
    accepted.sender.send(ReviewDecision::Approved).unwrap();
    assert_eq!(waiter.wait().await, ReviewDecision::Approved);
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn clearing_a_claim_then_reusing_the_key_preserves_the_replacement() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    start_approval_turn(&session, &turn, &events).await;
    let old_waiter = register(&session, &turn, /*deadline*/ None).await;
    let old = response(&turn, "old", ReviewDecision::Approved);
    let old_claim = session.claim_command_approval(&old).await.unwrap().unwrap();
    let new_waiter = register(&session, &turn, /*deadline*/ None).await;
    let new = response(&turn, "new", ReviewDecision::Approved);
    let new_claim = session.claim_command_approval(&new).await.unwrap().unwrap();

    assert!(
        session
            .consume_command_approval("approval", "old", old_claim)
            .await
            .is_none()
    );
    assert!(session.claim_command_approval(&old).await.is_err());
    let accepted = session
        .consume_command_approval("approval", "new", new_claim)
        .await
        .unwrap();
    drop(accepted.active_guard);
    accepted.sender.send(ReviewDecision::Approved).unwrap();
    assert_eq!(old_waiter.wait().await, ReviewDecision::Abort);
    assert_eq!(new_waiter.wait().await, ReviewDecision::Approved);
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn stale_abort_and_policy_decisions_cannot_affect_a_replacement_turn() {
    for decision in [
        ReviewDecision::Abort,
        ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment: ExecPolicyAmendment::new(vec![
                "approval-policy-sentinel".into(),
            ]),
        },
    ] {
        let (session, turn, events) = make_session_and_context_with_rx().await;
        start_approval_turn(&session, &turn, &events).await;
        let old_waiter = register(&session, &turn, /*deadline*/ None).await;
        let old = response(&turn, "old", decision.clone());
        let old_claim = session.claim_command_approval(&old).await.unwrap();
        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
        // Even identical external turn/item keys cannot authorize a different TurnState.
        start_approval_turn(&session, &turn, &events).await;
        let new_waiter = register(&session, &turn, /*deadline*/ None).await;
        let policy_before = session.services.exec_policy.current();
        let policy_path = crate::exec_policy::default_policy_path(turn.config.codex_home.as_path());
        let disk_before = tokio::fs::read(&policy_path).await.ok();

        handlers::exec_approval(
            &session,
            "approval".into(),
            Some(turn.sub_id.clone()),
            decision,
            "old",
            old_claim,
        )
        .await;

        assert!(Arc::ptr_eq(
            &policy_before,
            &session.services.exec_policy.current()
        ));
        assert_eq!(disk_before, tokio::fs::read(&policy_path).await.ok());
        assert!(
            session
                .is_root_command_approval("approval", &turn.sub_id)
                .await
        );
        assert_eq!(old_waiter.wait().await, ReviewDecision::Abort);
        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
        assert_eq!(new_waiter.wait().await, ReviewDecision::Abort);
    }
}

#[tokio::test]
async fn unclaimed_raw_decision_cannot_persist_policy_or_interrupt() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    start_approval_turn(&session, &turn, &events).await;
    let waiter = register(&session, &turn, /*deadline*/ None).await;
    for decision in [
        ReviewDecision::Abort,
        ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment: ExecPolicyAmendment::new(vec![
                "unclaimed-policy-sentinel".into(),
            ]),
        },
    ] {
        let policy = session.services.exec_policy.current();
        handlers::exec_approval(
            &session,
            "approval".into(),
            Some(turn.sub_id.clone()),
            decision,
            "raw",
            /*claim*/ None,
        )
        .await;
        assert!(Arc::ptr_eq(
            &policy,
            &session.services.exec_policy.current()
        ));
        assert!(
            session
                .is_root_command_approval("approval", &turn.sub_id)
                .await
        );
    }
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    assert_eq!(waiter.wait().await, ReviewDecision::Abort);
}
