//! Exact-request authority for human command responses, independent of queue latency.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::Weak;

use crate::session::Submission;
use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Session;
use super::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::NetworkApprovalPrompt;
use crate::state::TurnState;

/// A Code Mode callback keeps its issuing policy and presentation turn, while a
/// later worker supplies authority to one exact live task. Never serialized.
#[derive(Default)]
struct CodeModeCommandApprovalBindings(
    StdMutex<HashMap<(String, String), Weak<CodeModeCommandApprovalBinding>>>,
);

pub(super) struct CodeModeCommandApprovalBinding {
    turn: Weak<Mutex<TurnState>>,
    servicing_turn: Weak<TurnContext>,
    invocation_cancelled: CancellationToken,
    released: CancellationToken,
}

impl CodeModeCommandApprovalBinding {
    fn is_live(&self) -> bool {
        !self.invocation_cancelled.is_cancelled() && !self.released.is_cancelled()
    }

    pub(super) async fn cancelled(&self) {
        tokio::select! {
            () = self.invocation_cancelled.cancelled() => {}
            () = self.released.cancelled() => {}
        }
    }
}

pub(crate) struct CodeModeCommandApprovalLease {
    bindings: Arc<CodeModeCommandApprovalBindings>,
    key: (String, String),
    binding: Arc<CodeModeCommandApprovalBinding>,
}

impl Drop for CodeModeCommandApprovalLease {
    fn drop(&mut self) {
        self.binding.released.cancel();
        let mut bindings = self
            .bindings
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if bindings
            .get(&self.key)
            .is_some_and(|binding| binding.ptr_eq(&Arc::downgrade(&self.binding)))
        {
            bindings.remove(&self.key);
        }
    }
}

impl Session {
    pub(crate) async fn bind_code_mode_command_approval(
        &self,
        servicing_turn: &Arc<TurnContext>,
        presentation_turn: &TurnContext,
        call_id: &str,
        invocation_cancelled: CancellationToken,
    ) -> CodexResult<CodeModeCommandApprovalLease> {
        let active = self.active_turn.lock().await;
        let active = active.as_ref().ok_or_else(invalid_code_mode_servicer)?;
        let task = active
            .task
            .as_ref()
            .ok_or_else(invalid_code_mode_servicer)?;
        if task.cancellation_token.is_cancelled()
            || invocation_cancelled.is_cancelled()
            || !Arc::ptr_eq(&task.turn_context, servicing_turn)
        {
            return Err(invalid_code_mode_servicer());
        }
        let bindings = self
            .services
            .thread_extension_data
            .get_or_init(CodeModeCommandApprovalBindings::default);
        let key = (presentation_turn.sub_id.clone(), call_id.to_owned());
        let binding = Arc::new(CodeModeCommandApprovalBinding {
            turn: Arc::downgrade(&active.turn_state),
            servicing_turn: Arc::downgrade(&task.turn_context),
            invocation_cancelled,
            released: CancellationToken::new(),
        });
        let mut entries = bindings.0.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.get(&key).and_then(Weak::upgrade).is_some() {
            return Err(CodexErr::InvalidRequest(
                "code mode command approval call is already active".into(),
            ));
        }
        entries.insert(key.clone(), Arc::downgrade(&binding));
        drop(entries);
        Ok(CodeModeCommandApprovalLease {
            bindings,
            key,
            binding,
        })
    }

    pub(super) fn code_mode_command_approval_binding(
        &self,
        active: &ActiveTurn,
        presentation_turn_id: &str,
        call_id: &str,
    ) -> Option<Arc<CodeModeCommandApprovalBinding>> {
        let binding = self
            .services
            .thread_extension_data
            .get::<CodeModeCommandApprovalBindings>()
            .and_then(|bindings| {
                bindings
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&(presentation_turn_id.to_owned(), call_id.to_owned()))
                    .and_then(Weak::upgrade)
            })?;
        (binding.is_live()
            && binding.turn.ptr_eq(&Arc::downgrade(&active.turn_state))
            && active.task.as_ref().is_some_and(|task| {
                binding
                    .servicing_turn
                    .ptr_eq(&Arc::downgrade(&task.turn_context))
            }))
        .then_some(binding)
    }
}

fn invalid_code_mode_servicer() -> CodexErr {
    CodexErr::InvalidRequest("code mode servicing task is no longer active".into())
}

/// Never serialized: callback identity does not encode command presentation ownership.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandApprovalOrigin {
    RootCommand,
    HumanCommand,
    NestedExecve,
    WriteStdin,
    Network,
}

pub(crate) struct QueuedSubmission {
    pub(crate) submission: Submission,
    pub(crate) approval: Option<CommandApprovalClaim>,
    /// Non-serializable authority captured before a user-shell submission is enqueued.
    pub(crate) user_shell_completion: Option<super::AcceptedCompletionDelivery>,
}

impl std::fmt::Debug for QueuedSubmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedSubmission")
            .field("submission", &self.submission)
            .field("has_approval_claim", &self.approval.is_some())
            .field(
                "has_user_shell_completion",
                &self.user_shell_completion.is_some(),
            )
            .finish()
    }
}

impl From<Submission> for QueuedSubmission {
    fn from(submission: Submission) -> Self {
        Self {
            submission,
            approval: None,
            user_shell_completion: None,
        }
    }
}

pub(crate) struct PendingCommandApproval(Arc<ApprovalCell>);

/// A cancellation guard may remove only the registration that created it.
pub(crate) struct CommandApprovalIdentity(Weak<ApprovalCell>);

impl CommandApprovalIdentity {
    pub(crate) fn matches(&self, pending: &PendingCommandApproval) -> bool {
        self.0.ptr_eq(&Arc::downgrade(&pending.0))
    }
}

/// The request future owns cancellation even while a queued claim retains the cell.
pub(crate) struct CommandApprovalWaiter {
    cell: Arc<ApprovalCell>,
    receiver: oneshot::Receiver<ReviewDecision>,
    // Dropped after the waiter invalidates its cell, before the next endpoint prompt.
    _network_prompt: Option<NetworkApprovalPrompt>,
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
    pub(crate) servicing_turn_id: String,
    pub(crate) kind: ExecApprovalKind,
    pub(crate) codex_home: AbsolutePathBuf,
    /// Policy persistence must finish before the originating active turn can be replaced.
    pub(crate) active_guard: tokio::sync::MutexGuard<'a, Option<ActiveTurn>>,
}

struct ApprovalCell {
    turn: Weak<Mutex<TurnState>>,
    turn_id: String,
    servicing_turn_id: String,
    kind: ExecApprovalKind,
    origin: CommandApprovalOrigin,
    deadline: Option<Instant>,
    code_mode_binding: OnceLock<Weak<CodeModeCommandApprovalBinding>>,
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
        servicing_turn_id: String,
        kind: ExecApprovalKind,
        origin: CommandApprovalOrigin,
        deadline: Option<Instant>,
    ) -> (Self, CommandApprovalWaiter) {
        let (sender, receiver) = oneshot::channel();
        let cell = Arc::new(ApprovalCell {
            turn: Arc::downgrade(turn),
            turn_id,
            servicing_turn_id,
            kind,
            origin,
            deadline,
            code_mode_binding: OnceLock::new(),
            state: StdMutex::new(ApprovalState {
                phase: Phase::Pending,
                sender: Some(sender),
            }),
        });
        (
            Self(Arc::clone(&cell)),
            CommandApprovalWaiter {
                cell,
                receiver,
                _network_prompt: None,
            },
        )
    }

    pub(super) fn bind_code_mode(&self, binding: &Arc<CodeModeCommandApprovalBinding>) {
        assert!(
            self.0
                .code_mode_binding
                .set(Arc::downgrade(binding))
                .is_ok()
        );
    }

    pub(crate) fn identity(&self) -> CommandApprovalIdentity {
        CommandApprovalIdentity(Arc::downgrade(&self.0))
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
                .is_some_and(|task| task.turn_context.sub_id == self.0.servicing_turn_id)
    }
}

impl ApprovalCell {
    fn code_mode_binding_is_live(&self) -> bool {
        self.code_mode_binding
            .get()
            .is_none_or(|binding| binding.upgrade().is_some_and(|binding| binding.is_live()))
    }

    fn expired(&self) -> bool {
        !self.code_mode_binding_is_live()
            || self
                .deadline
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
    pub(crate) fn track_network_prompt(&mut self, prompt: NetworkApprovalPrompt) {
        self._network_prompt = Some(prompt);
    }

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
        let active = matching_turn(active.as_ref(), None)?;
        let state = active.turn_state.lock().await;
        let approvals = state
            .command_approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pending = approvals.get(id).ok_or_else(invalid_approval)?;
        if turn_id.as_deref().is_some_and(|id| id != pending.0.turn_id)
            || !pending.belongs_to(active)
        {
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
        let active = matching_turn(active.as_ref(), None)?;
        let state = active
            .turn_state
            .try_lock()
            .map_err(|_| invalid_approval())?;
        let approvals = state
            .command_approvals
            .try_lock()
            .map_err(|_| invalid_approval())?;
        let pending = approvals.get(id).ok_or_else(invalid_approval)?;
        if turn_id.as_deref().is_some_and(|id| id != pending.0.turn_id)
            || !pending.belongs_to(active)
        {
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
        let active =
            matching_turn(active_guard.as_ref(), Some(&claim.cell.servicing_turn_id)).ok()?;
        let codex_home = active.task.as_ref()?.turn_context.config.codex_home.clone();
        let origin = claim.cell.turn.upgrade()?;
        if !Arc::ptr_eq(&origin, &active.turn_state) || claim.submission_id != submission_id {
            return None;
        }
        let turn = origin.lock().await;
        let mut approvals = turn
            .command_approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pending = approvals.get(approval_id)?;
        if !Arc::ptr_eq(&pending.0, &claim.cell) || !claim.cell.code_mode_binding_is_live() {
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
        approvals.remove(approval_id);
        drop(approvals);
        drop(turn);
        sender.map(|sender| AcceptedCommandApproval {
            sender,
            turn: Arc::clone(&origin),
            turn_id: claim.cell.turn_id.clone(),
            servicing_turn_id: claim.cell.servicing_turn_id.clone(),
            kind: claim.cell.kind,
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
        let Ok(active) = matching_turn(active.as_ref(), None) else {
            return false;
        };
        let state = active.turn_state.lock().await;
        let approvals = state
            .command_approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        approvals.get(approval_id).is_some_and(|pending| {
            pending.0.turn_id == turn_id && pending.belongs_to(active) && pending.is_live_root()
        })
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
