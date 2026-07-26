//! Exact-request authority for human command responses, independent of queue latency.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::Weak;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::Submission;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::Session;
use crate::state::ActiveTurn;
use crate::state::TurnState;

/// Never serialized: callback identity does not encode command presentation ownership.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandApprovalOrigin {
    RootCommand,
    NestedExecve,
    WriteStdin,
    Network,
}

pub(crate) struct QueuedSubmission {
    pub(crate) submission: Submission,
    pub(crate) approval: Option<CommandApprovalClaim>,
}

impl std::fmt::Debug for QueuedSubmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedSubmission")
            .field("submission", &self.submission)
            .field("has_approval_claim", &self.approval.is_some())
            .finish()
    }
}

impl From<Submission> for QueuedSubmission {
    fn from(submission: Submission) -> Self {
        Self {
            submission,
            approval: None,
        }
    }
}

pub(crate) struct PendingCommandApproval(Arc<ApprovalCell>);

/// The request future owns cancellation even while a queued claim retains the cell.
pub(crate) struct CommandApprovalWaiter {
    cell: Arc<ApprovalCell>,
    receiver: oneshot::Receiver<ReviewDecision>,
}

/// Non-cloneable authority transferred from the submitting future into the queue.
pub(crate) struct CommandApprovalClaim {
    cell: Arc<ApprovalCell>,
    submission_id: String,
}

pub(crate) struct AcceptedCommandApproval<'a> {
    pub(crate) sender: oneshot::Sender<ReviewDecision>,
    pub(crate) turn: Arc<Mutex<TurnState>>,
    pub(crate) turn_id: String,
    pub(crate) codex_home: AbsolutePathBuf,
    /// Policy persistence must finish before the originating active turn can be replaced.
    pub(crate) active_guard: tokio::sync::MutexGuard<'a, Option<ActiveTurn>>,
}

struct ApprovalCell {
    turn: Weak<Mutex<TurnState>>,
    turn_id: String,
    origin: CommandApprovalOrigin,
    deadline: Option<Instant>,
    state: StdMutex<ApprovalState>,
}

struct ApprovalState {
    phase: Phase,
    sender: Option<oneshot::Sender<ReviewDecision>>,
}

enum Phase {
    Pending,
    Claimed(String),
    Finished,
}

impl PendingCommandApproval {
    pub(crate) fn new(
        turn: &Arc<Mutex<TurnState>>,
        turn_id: String,
        origin: CommandApprovalOrigin,
        deadline: Option<Instant>,
    ) -> (Self, CommandApprovalWaiter) {
        let (sender, receiver) = oneshot::channel();
        let cell = Arc::new(ApprovalCell {
            turn: Arc::downgrade(turn),
            turn_id,
            origin,
            deadline,
            state: StdMutex::new(ApprovalState {
                phase: Phase::Pending,
                sender: Some(sender),
            }),
        });
        (
            Self(Arc::clone(&cell)),
            CommandApprovalWaiter { cell, receiver },
        )
    }

    fn claim(&self, submission_id: &str) -> CodexResult<CommandApprovalClaim> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(state.phase, Phase::Pending) || self.0.expired() {
            return Err(invalid_approval());
        }
        state.phase = Phase::Claimed(submission_id.to_owned());
        Ok(CommandApprovalClaim {
            cell: Arc::clone(&self.0),
            submission_id: submission_id.to_owned(),
        })
    }

    fn is_live_root(&self) -> bool {
        self.0.origin == CommandApprovalOrigin::RootCommand
            && !self.0.expired()
            && matches!(
                self.0
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase,
                Phase::Pending
            )
    }

    fn belongs_to(&self, active: &ActiveTurn) -> bool {
        self.0.turn.ptr_eq(&Arc::downgrade(&active.turn_state))
            && active
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == self.0.turn_id)
    }
}

impl ApprovalCell {
    fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.phase = Phase::Finished;
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(ReviewDecision::Abort);
        }
    }

    fn expire_unclaimed(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(state.phase, Phase::Pending) {
            state.phase = Phase::Finished;
            if let Some(sender) = state.sender.take() {
                let _ = sender.send(ReviewDecision::TimedOut);
            }
        }
    }
}

impl Drop for PendingCommandApproval {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl CommandApprovalWaiter {
    pub(crate) async fn wait(mut self) -> ReviewDecision {
        if let Some(deadline) = self.cell.deadline {
            tokio::select! {
                biased;
                decision = &mut self.receiver => return decision.unwrap_or(ReviewDecision::Abort),
                () = tokio::time::sleep_until(deadline) => self.cell.expire_unclaimed(),
            }
        }
        // A timely claim owns resolution after the deadline. Its cancellation
        // synchronously releases or expires it, so this wait cannot be stranded.
        (&mut self.receiver).await.unwrap_or(ReviewDecision::Abort)
    }
}

impl Drop for CommandApprovalWaiter {
    fn drop(&mut self) {
        self.cell.cancel();
    }
}

impl Drop for CommandApprovalClaim {
    fn drop(&mut self) {
        let mut state = self
            .cell
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(&state.phase, Phase::Claimed(id) if id == &self.submission_id) {
            if self.cell.expired() {
                state.phase = Phase::Finished;
                if let Some(sender) = state.sender.take() {
                    let _ = sender.send(ReviewDecision::TimedOut);
                }
            } else {
                state.phase = Phase::Pending;
            }
        }
    }
}

impl Session {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "approval origin and registration must be checked atomically"
    )]
    pub(crate) async fn claim_command_approval(
        &self,
        submission: &Submission,
    ) -> CodexResult<Option<CommandApprovalClaim>> {
        let Op::ExecApproval { id, turn_id, .. } = &submission.op else {
            return Ok(None);
        };
        let active = self.active_turn.lock().await;
        let active = matching_turn(active.as_ref(), turn_id.as_deref())?;
        let state = active.turn_state.lock().await;
        let pending = state
            .command_approvals
            .get(id)
            .ok_or_else(invalid_approval)?;
        if !pending.belongs_to(active) {
            return Err(invalid_approval());
        }
        pending.claim(&submission.id).map(Some)
    }

    pub(crate) fn try_claim_command_approval(
        &self,
        submission: &Submission,
    ) -> CodexResult<Option<CommandApprovalClaim>> {
        let Op::ExecApproval { id, turn_id, .. } = &submission.op else {
            return Ok(None);
        };
        let active = self
            .active_turn
            .try_lock()
            .map_err(|_| invalid_approval())?;
        let active = matching_turn(active.as_ref(), turn_id.as_deref())?;
        let state = active
            .turn_state
            .try_lock()
            .map_err(|_| invalid_approval())?;
        let pending = state
            .command_approvals
            .get(id)
            .ok_or_else(invalid_approval)?;
        if !pending.belongs_to(active) {
            return Err(invalid_approval());
        }
        pending.claim(&submission.id).map(Some)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "claim consumption must validate the originating active turn atomically"
    )]
    pub(crate) async fn consume_command_approval(
        &self,
        approval_id: &str,
        submission_id: &str,
        claim: CommandApprovalClaim,
    ) -> Option<AcceptedCommandApproval<'_>> {
        let active_guard = self.active_turn.lock().await;
        let active = matching_turn(active_guard.as_ref(), Some(&claim.cell.turn_id)).ok()?;
        let codex_home = active.task.as_ref()?.turn_context.config.codex_home.clone();
        let origin = claim.cell.turn.upgrade()?;
        if !Arc::ptr_eq(&origin, &active.turn_state) || claim.submission_id != submission_id {
            return None;
        }
        let mut turn = origin.lock().await;
        let pending = turn.command_approvals.get(approval_id)?;
        if !Arc::ptr_eq(&pending.0, &claim.cell) {
            return None;
        }
        let sender = {
            let mut state = claim
                .cell
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !matches!(&state.phase, Phase::Claimed(id) if id == submission_id) {
                return None;
            }
            state.phase = Phase::Finished;
            state.sender.take()
        };
        turn.command_approvals.remove(approval_id);
        drop(turn);
        sender.map(|sender| AcceptedCommandApproval {
            sender,
            turn: Arc::clone(&origin),
            turn_id: claim.cell.turn_id.clone(),
            codex_home,
            active_guard,
        })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "presentation provenance belongs to the exact active turn"
    )]
    pub(crate) async fn is_root_command_approval(&self, approval_id: &str, turn_id: &str) -> bool {
        let active = self.active_turn.lock().await;
        let Ok(active) = matching_turn(active.as_ref(), Some(turn_id)) else {
            return false;
        };
        active
            .turn_state
            .lock()
            .await
            .command_approvals
            .get(approval_id)
            .is_some_and(|pending| pending.belongs_to(active) && pending.is_live_root())
    }
}

fn matching_turn<'a>(
    active: Option<&'a ActiveTurn>,
    turn_id: Option<&str>,
) -> CodexResult<&'a ActiveTurn> {
    let active = active.ok_or_else(invalid_approval)?;
    let task = active.task.as_ref().ok_or_else(invalid_approval)?;
    if task.cancellation_token.is_cancelled()
        || turn_id.is_some_and(|id| id != task.turn_context.sub_id)
    {
        return Err(invalid_approval());
    }
    Ok(active)
}

fn invalid_approval() -> CodexErr {
    CodexErr::InvalidRequest(
        "command approval is stale, expired, already claimed, or unavailable".into(),
    )
}

pub(crate) fn saturating_instant_add_ms(now: Instant, timeout_ms: u64) -> Instant {
    if let Some(deadline) = now.checked_add(std::time::Duration::from_millis(timeout_ms)) {
        return deadline;
    }
    let mut lower = 0;
    let mut upper = timeout_ms;
    while lower < upper {
        let midpoint = lower + (upper - lower).div_ceil(2);
        if now
            .checked_add(std::time::Duration::from_millis(midpoint))
            .is_some()
        {
            lower = midpoint;
        } else {
            upper = midpoint - 1;
        }
    }
    now.checked_add(std::time::Duration::from_millis(lower))
        .unwrap_or(now)
}

#[cfg(test)]
#[path = "command_approval_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "command_approval_session_tests.rs"]
mod session_tests;
