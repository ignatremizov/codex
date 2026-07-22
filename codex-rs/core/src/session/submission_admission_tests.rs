use super::*;
use crate::session::SessionIo;
use crate::session::completed_session_loop_termination;
use codex_protocol::protocol::AgentStatus;
use pretty_assertions::assert_eq;
use std::time::Duration as StdDuration;
use tokio::sync::watch;

fn submission(id: &str, op: Op) -> Submission {
    Submission {
        id: id.to_string(),
        op,
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
    }
}

#[test]
fn try_enqueue_rejects_held_send_order_without_reserving() {
    let (sender, receiver) = async_channel::bounded(1);
    let admission = Arc::new(SubmissionAdmission::default());
    let order = admission.send_lock.try_lock().expect("reserve send order");
    let error = admission
        .try_enqueue(
            &sender,
            submission("rollback", Op::ThreadRollback { num_turns: 1 }),
        )
        .expect_err("must reject without waiting for send order");
    assert!(matches!(error, CodexErr::InvalidRequest(_)));
    assert!(admission.check_ready().is_ok());
    assert!(receiver.try_recv().is_err());
    drop(order);
    admission
        .try_enqueue(
            &sender,
            submission("accepted", Op::ThreadRollback { num_turns: 1 }),
        )
        .expect("lock rejection must not retain a reservation");
    assert_eq!(
        receiver.try_recv().expect("accepted rollback").id,
        "accepted"
    );
}

#[test]
fn try_enqueue_full_queue_releases_only_the_failed_reservation() {
    let (sender, receiver) = async_channel::bounded(1);
    let admission = Arc::new(SubmissionAdmission::default());
    sender
        .try_send(submission("before", Op::Compact))
        .expect("fill queue");
    let error = admission
        .try_enqueue(
            &sender,
            submission("rejected", Op::ThreadRollback { num_turns: 1 }),
        )
        .expect_err("must reject full queue");
    assert!(matches!(error, CodexErr::InvalidRequest(_)));
    assert!(admission.check_ready().is_ok());
    assert_eq!(
        receiver.try_recv().expect("original submission").id,
        "before"
    );
    assert!(receiver.try_recv().is_err());
    admission
        .try_enqueue(
            &sender,
            submission("accepted", Op::ThreadRollback { num_turns: 1 }),
        )
        .expect("a later attempt can reserve available capacity");
    admission.rollback_completed("rejected");
    assert!(admission.check_ready().is_err());
    assert_eq!(
        receiver.try_recv().expect("accepted rollback").id,
        "accepted"
    );
}

#[test]
fn try_enqueue_closed_queue_releases_the_failed_reservation() {
    let (sender, receiver) = async_channel::bounded(1);
    let admission = Arc::new(SubmissionAdmission::default());
    drop(receiver);
    let error = admission
        .try_enqueue(
            &sender,
            submission("rejected", Op::ThreadRollback { num_turns: 1 }),
        )
        .expect_err("closed queue cannot accept work");
    assert!(matches!(error, CodexErr::InternalAgentDied));
    assert!(admission.check_ready().is_ok());
}

#[test]
fn try_submit_preserves_trace_and_correlates_accepted_reservation() {
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    let (_tx_event, rx_event) = async_channel::unbounded();
    let io = SessionIo {
        tx_sub,
        rx_event,
        submission_admission: Arc::new(SubmissionAdmission::default()),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: completed_session_loop_termination(),
    };
    let _tracing = core_test_support::tracing::install_test_tracing("codex-core-tests");
    let span = tracing::info_span!("try_submit_request");
    assert!(codex_otel::set_parent_from_w3c_trace_context(
        &span,
        &codex_protocol::protocol::W3cTraceContext {
            traceparent: Some(
                "00-00000000000000000000000000000011-0000000000000022-01".to_string()
            ),
            tracestate: Some("vendor=value".to_string()),
        },
    ));
    let (id, expected_trace) = span.in_scope(|| {
        let trace = codex_otel::current_span_w3c_trace_context().expect("active trace");
        (
            io.try_submit(Op::ThreadRollback { num_turns: 1 })
                .expect("accept rollback"),
            trace,
        )
    });
    let accepted = rx_sub.try_recv().expect("accepted rollback");
    assert!(matches!(accepted.op, Op::ThreadRollback { num_turns: 1 }));
    assert_eq!(
        (
            accepted.id,
            accepted.trace,
            accepted.parent_turn_id,
            accepted.root_turn_id
        ),
        (id.clone(), Some(expected_trace), None, None),
    );
    assert!(io.try_submit(Op::Compact).is_err());
    io.submission_admission
        .rollback_completed("another-submission");
    assert!(io.submission_admission.check_ready().is_err());
    io.submission_admission.rollback_completed(&id);
    assert!(io.submission_admission.check_ready().is_ok());
}

#[test]
fn failed_reservation_cleanup_and_try_enqueue_preserve_quarantine() {
    let (sender, receiver) = async_channel::bounded(1);
    let admission = Arc::new(SubmissionAdmission::default());
    let rollback = submission("rollback", Op::ThreadRollback { num_turns: 1 });
    let order = admission.send_lock.try_lock().expect("reserve send order");
    let reservation = admission.reserve(&rollback).expect("reserve rollback");
    admission.rollback_requires_reload();
    // This is the same RAII cleanup used when either send variant rejects the command.
    drop(reservation);
    drop(order);
    assert!(admission.requires_reload());
    assert!(admission.try_enqueue(&sender, rollback).is_err());
    assert!(receiver.try_recv().is_err());
    assert!(admission.requires_reload());
    admission
        .try_enqueue(&sender, submission("shutdown", Op::Shutdown))
        .expect("quarantined shutdown is still admitted");
    assert_eq!(receiver.try_recv().expect("shutdown").id, "shutdown");
    assert!(admission.requires_reload());
}

#[tokio::test]
async fn submission_admission_rejects_work_queued_behind_rollback() {
    let (tx_sub, rx_sub) = async_channel::bounded(2);
    let (_tx_event, rx_event) = async_channel::unbounded();
    let io = SessionIo {
        tx_sub,
        rx_event,
        submission_admission: Arc::new(SubmissionAdmission::default()),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: completed_session_loop_termination(),
    };

    io.submit(Op::ThreadRollback { num_turns: 1 })
        .await
        .expect("rollback should reserve submission admission");
    let err = io
        .submit(Op::Compact)
        .await
        .expect_err("work behind a pending rollback should be rejected");
    assert!(
        matches!(err, CodexErr::InvalidRequest(message) if message == "thread rollback is already in progress")
    );

    let queued = rx_sub.recv().await.expect("rollback should be queued");
    assert!(matches!(queued.op, Op::ThreadRollback { num_turns: 1 }));
    assert!(matches!(
        rx_sub.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn submission_admission_transition_does_not_wait_for_channel_capacity() {
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    let (_tx_event, rx_event) = async_channel::unbounded();
    let io = Arc::new(SessionIo {
        tx_sub,
        rx_event,
        submission_admission: Arc::new(SubmissionAdmission::default()),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: completed_session_loop_termination(),
    });

    io.submit(Op::ThreadRollback { num_turns: 1 })
        .await
        .expect("rollback should fill the submission channel");
    let shutdown = {
        let io = Arc::clone(&io);
        tokio::spawn(async move { io.submit(Op::Shutdown).await })
    };
    tokio::time::timeout(StdDuration::from_millis(100), async {
        loop {
            if io.submission_admission.send_lock.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown should block while holding the send-order lock");

    io.submission_admission.rollback_requires_reload();

    let rollback = rx_sub.recv().await.expect("rollback should be queued");
    assert!(matches!(rollback.op, Op::ThreadRollback { num_turns: 1 }));
    shutdown
        .await
        .expect("shutdown task should not panic")
        .expect("shutdown should queue after capacity is released");
    let shutdown = rx_sub.recv().await.expect("shutdown should be queued");
    assert!(matches!(shutdown.op, Op::Shutdown));
}

#[tokio::test]
async fn cancelling_blocked_rollback_submission_releases_admission() {
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    let (_tx_event, rx_event) = async_channel::unbounded();
    let io = Arc::new(SessionIo {
        tx_sub,
        rx_event,
        submission_admission: Arc::new(SubmissionAdmission::default()),
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: completed_session_loop_termination(),
    });

    io.submit(Op::Interrupt)
        .await
        .expect("interrupt should fill the submission channel");
    let rollback = {
        let io = Arc::clone(&io);
        tokio::spawn(async move { io.submit(Op::ThreadRollback { num_turns: 1 }).await })
    };
    tokio::time::timeout(StdDuration::from_millis(100), async {
        loop {
            if io.submission_admission.send_lock.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rollback should block while holding the send-order lock");

    rollback.abort();
    let _ = rollback.await;
    assert!(io.submission_admission.check_ready().is_ok());

    let queued = rx_sub.recv().await.expect("interrupt should be queued");
    assert!(matches!(queued.op, Op::Interrupt));
    io.submit(Op::Compact)
        .await
        .expect("admission should accept work after rollback cancellation");
}

#[tokio::test]
async fn quarantine_rejects_reply_bearing_submissions_and_stale_completion() {
    let (sender, receiver) = async_channel::bounded(4);
    let admission = Arc::new(SubmissionAdmission::default());
    let submission = |id: &str, op| Submission {
        id: id.to_string(),
        op,
        trace: None,
        parent_turn_id: None,
        root_turn_id: None,
    };
    admission
        .enqueue(
            &sender,
            submission("rollback", Op::ThreadRollback { num_turns: 1 }),
        )
        .await
        .expect("reserve rollback");
    let (reply, result) = tokio::sync::oneshot::channel();
    assert!(
        admission
            .enqueue(
                &sender,
                submission("suspend", Op::SuspendTurnAndShutdown { reply })
            )
            .await
            .is_err()
    );
    assert!(result.await.is_err());
    admission.rollback_completed("different-submission");
    assert!(admission.check_ready().is_err());
    admission.rollback_requires_reload();
    admission.rollback_completed("rollback");
    assert!(admission.requires_reload());
    assert!(!admission.can_reload());
    admission.acknowledge_writer_closed();
    assert!(admission.can_reload());
    assert!(admission.admit_injection().await.is_err());
    admission
        .enqueue(&sender, submission("shutdown", Op::Shutdown))
        .await
        .expect("shutdown remains admitted");
    assert_eq!(receiver.recv().await.expect("rollback").id, "rollback");
    assert_eq!(receiver.recv().await.expect("shutdown").id, "shutdown");
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn cancelled_reservation_cannot_reopen_quarantine() {
    let (sender, receiver) = async_channel::bounded(1);
    let admission = Arc::new(SubmissionAdmission::default());
    sender
        .send(Submission {
            id: "before".to_string(),
            op: Op::Compact,
            trace: None,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await
        .expect("fill channel");
    let mut pending = Box::pin(admission.enqueue(
        &sender,
        Submission {
            id: "rollback".to_string(),
            op: Op::ThreadRollback { num_turns: 1 },
            trace: None,
            parent_turn_id: None,
            root_turn_id: None,
        },
    ));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    admission.rollback_requires_reload();
    drop(pending);
    assert!(admission.requires_reload());
    assert_eq!(receiver.recv().await.expect("original work").id, "before");
    assert!(receiver.try_recv().is_err());
}
