//! Shared command-approval fixtures use real task registration and response admission.

use std::sync::Arc;

use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use tokio::sync::Notify;

use super::Session;
use super::SessionIo;
use super::completed_session_loop_termination;
use super::handlers;
use super::tests::HeldStepTask;
use super::turn_context::TurnContext;
use crate::state::TaskKind;

pub(crate) async fn start_approval_turn(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    events: &async_channel::Receiver<Event>,
) {
    session
        .spawn_task(
            Arc::clone(turn),
            Vec::new(),
            HeldStepTask {
                kind: TaskKind::Regular,
                finish: Arc::new(Notify::new()),
            },
        )
        .await;
    // These fixtures begin their command after registration; discard only setup events.
    while events.try_recv().is_ok() {}
}

pub(crate) async fn respond_to_approval(
    session: &Arc<Session>,
    event: &ExecApprovalRequestEvent,
    decision: ReviewDecision,
) {
    let (tx_sub, rx_sub) = async_channel::bounded(/*cap*/ 1);
    let (_tx_event, rx_event) = async_channel::unbounded();
    let io = SessionIo {
        tx_sub,
        session: Arc::downgrade(session),
        rx_event,
        submission_admission: Arc::clone(&session.submission_admission),
        agent_status: tokio::sync::watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: completed_session_loop_termination(),
    };
    let approval_id = event.effective_approval_id().to_owned();
    let submission_id = io
        .submit(Op::ExecApproval {
            id: approval_id.clone(),
            turn_id: Some(event.turn_id.clone()),
            decision: decision.clone(),
        })
        .await
        .expect("live approval should be accepted");
    let queued = rx_sub.recv().await.expect("accepted approval");
    handlers::exec_approval(
        session,
        approval_id,
        Some(event.turn_id.clone()),
        decision,
        &submission_id,
        queued.approval,
    )
    .await;
}
