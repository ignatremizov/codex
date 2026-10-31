use super::WaitReturnReason;
use super::wait_for_outcome;
use crate::session::TerminalStatusEvent;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TurnAbortReason;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct PendingTask;

impl SessionTask for PendingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.wait_input_test"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        cancellation_token.cancelled().await;
        Err(codex_protocol::error::CodexErr::TurnAborted)
    }
}

async fn active_waiter() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), PendingTask)
        .await;
    (session, turn)
}

#[derive(Clone, Copy)]
enum InputTiming {
    Pending,
    DuringWait,
}

#[test_case(InputTiming::Pending; "pending_before_subscription")]
#[test_case(InputTiming::DuringWait; "accepted_after_subscription")]
#[tokio::test]
async fn input_releases_completion_wait_without_consuming_it(
    timing: InputTiming,
) -> anyhow::Result<()> {
    let (session, turn) = active_waiter().await;
    let (target, _) = make_session_and_context().await;
    let (_, subscription) = target.subscribe_terminal_status_events();
    let turn_state = session
        .input_queue
        .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("active waiter turn"))?;
    let wait = wait_for_outcome(
        &session,
        &turn.sub_id,
        vec![(target.thread_id, subscription)],
        Vec::new(),
        Instant::now() + Duration::from_secs(/*secs*/ 30),
        /*selection*/ None,
    );
    tokio::pin!(wait);
    if matches!(timing, InputTiming::DuringWait) {
        assert!(wait.as_mut().now_or_never().is_none());
    }
    session
        .route_realtime_text_input("Continue with this new input.".into())
        .await
        .map_err(anyhow::Error::msg)?;
    let pending = turn_state.lock().await.pending_input.as_slice().to_vec();
    assert!(!pending.is_empty());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(/*secs*/ 2), wait).await??,
        (Vec::new(), WaitReturnReason::Input),
    );
    assert_eq!(
        turn_state.lock().await.pending_input.as_slice(),
        pending.as_slice(),
    );
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
    Ok(())
}

#[tokio::test]
async fn input_for_a_later_turn_does_not_release_the_old_wait() -> anyhow::Result<()> {
    let (session, turn) = active_waiter().await;
    let (target, _) = make_session_and_context().await;
    let (_, subscription) = target.subscribe_terminal_status_events();
    {
        let wait = wait_for_outcome(
            &session,
            &turn.sub_id,
            vec![(target.thread_id, subscription)],
            Vec::new(),
            Instant::now() + Duration::from_secs(/*secs*/ 30),
            /*selection*/ None,
        );
        tokio::pin!(wait);
        assert!(wait.as_mut().now_or_never().is_none());
        session.abort_all_tasks(TurnAbortReason::Replaced).await;
        let next_turn = session.new_default_turn().await;
        session.spawn_task(next_turn, Vec::new(), PendingTask).await;
        session
            .route_realtime_text_input("Input for the replacement turn.".into())
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(wait.as_mut().now_or_never().is_none());
    }
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
    Ok(())
}

#[tokio::test]
async fn ready_completion_takes_precedence_over_pending_input() -> anyhow::Result<()> {
    let (session, turn) = active_waiter().await;
    session
        .route_realtime_text_input("Input remains pending after completion.".into())
        .await
        .map_err(anyhow::Error::msg)?;
    let completed = vec![(
        codex_protocol::ThreadId::new(),
        TerminalStatusEvent {
            turn_id: Some("completed-child-turn".into()),
            status: AgentStatus::Completed(Some("done".into())),
        },
    )];
    assert_eq!(
        wait_for_outcome(
            &session,
            &turn.sub_id,
            Vec::new(),
            completed.clone(),
            Instant::now() + Duration::from_secs(/*secs*/ 30),
            /*selection*/ None,
        )
        .await?,
        (completed, WaitReturnReason::Completion),
    );
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
    Ok(())
}
