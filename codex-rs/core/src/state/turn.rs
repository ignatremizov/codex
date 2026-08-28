//! Turn-scoped state and active turn metadata scaffolding.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use codex_diagnostics::GaugeGuard;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use rmcp::model::RequestId;
use tokio::sync::oneshot;

use crate::agent::control::AgentExecutionGuard;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::mcp_tool_call::McpToolApprovalMetadata;
use crate::session::TurnInput;
use crate::session::TurnInputQueue;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tasks::AnySessionTask;
use crate::tasks::TaskStartupState;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TokenUsage;

/// Metadata about the currently running turn.
pub(crate) struct ActiveTurn {
    pub(crate) task: Option<RunningTask>,
    pub(crate) turn_state: Arc<Mutex<TurnState>>,
}

/// Whether mailbox deliveries should still be folded into the current turn.
///
/// State machine:
/// - A turn starts in `CurrentTurn`, so queued child mail can join the next
///   model request for that turn.
/// - After user-visible terminal output is recorded, we switch to `NextTurn`
///   to leave late child mail queued instead of extending an already shown
///   answer.
/// - If the same task later gets explicit same-turn work again (a steered user
///   prompt or a tool call after an untagged preamble), we reopen `CurrentTurn`
///   so that pending child mail is drained into that follow-up request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MailboxDeliveryPhase {
    /// Incoming mailbox messages can still be consumed by the current turn.
    #[default]
    CurrentTurn,
    /// The current turn already emitted visible final answer text; mailbox
    /// messages should remain queued for a later turn.
    NextTurn,
}

impl Default for ActiveTurn {
    fn default() -> Self {
        Self {
            task: None,
            turn_state: Arc::new(Mutex::new(TurnState::default())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskKind {
    Regular,
    Review,
    Compact,
}

pub(crate) struct RunningTask {
    pub(crate) done: Arc<Notify>,
    pub(crate) startup: Arc<TaskStartupState>,
    pub(crate) kind: TaskKind,
    pub(crate) task: Arc<dyn AnySessionTask>,
    /// Whether this specific task invocation can still consume newly accepted pending input.
    pub(crate) accepting_pending_input: Arc<AtomicBool>,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) handle: AbortOnDropHandle<()>,
    pub(crate) turn_context: Arc<TurnContext>,
    pub(crate) input_persisted:
        Option<tokio::sync::oneshot::Sender<Result<(), TryStartTurnIfIdleRejectionReason>>>,
    pub(crate) _agent_execution_guard: Option<AgentExecutionGuard>,
    pub(crate) _diagnostics_guard: GaugeGuard,
    // Timer recorded when the task drops to capture the full turn duration.
    pub(crate) _timer: Option<codex_otel::Timer>,
}

/// Mutable state for a single turn.
#[derive(Default)]
pub(crate) struct TurnState {
    pending_approvals: HashMap<String, PendingApproval>,
    pending_request_permissions: HashMap<String, PendingRequestPermissions>,
    pending_user_input: HashMap<String, oneshot::Sender<RequestUserInputResponse>>,
    pending_elicitations: HashMap<(String, RequestId), oneshot::Sender<ElicitationResponse>>,
    mcp_tool_approval_metadata: HashMap<String, (Option<McpInvocation>, McpToolApprovalMetadata)>,
    pending_dynamic_tools: HashMap<String, oneshot::Sender<DynamicToolResponse>>,
    pub(crate) pending_input: TurnInputQueue,
    mailbox_delivery_phase: MailboxDeliveryPhase,
    granted_permissions_by_environment_id: HashMap<String, AdditionalPermissionProfile>,
    strict_auto_review_enabled: bool,
    pub(crate) tool_calls: u64,
    pub(crate) has_memory_citation: bool,
    pub(crate) token_usage_at_turn_start: TokenUsage,
    /// The last step captured for execution or selected from a speculative fallback.
    /// Remains absent until a step is captured; standalone local compaction has no step.
    pub(crate) last_known_step_context: Option<Arc<StepContext>>,
}

struct PendingApproval {
    identity: Arc<()>,
    deadline: Option<tokio::time::Instant>,
    claimed: bool,
    tx: oneshot::Sender<ReviewDecision>,
}

pub(crate) struct PendingRequestPermissions {
    pub(crate) tx_response: oneshot::Sender<RequestPermissionsResponse>,
    pub(crate) requested_permissions: RequestPermissionProfile,
    pub(crate) environment: TurnEnvironment,
}

impl TurnState {
    pub(crate) fn insert_pending_approval(
        &mut self,
        key: String,
        deadline: Option<tokio::time::Instant>,
        tx: oneshot::Sender<ReviewDecision>,
    ) -> Option<Arc<()>> {
        match self.pending_approvals.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => None,
            std::collections::hash_map::Entry::Vacant(entry) => {
                let identity = Arc::new(());
                entry.insert(PendingApproval {
                    identity: Arc::clone(&identity),
                    deadline,
                    claimed: false,
                    tx,
                });
                Some(identity)
            }
        }
    }

    pub(crate) fn claim_pending_approval(&mut self, key: &str) -> bool {
        let Some(pending) = self.pending_approvals.get_mut(key) else {
            return false;
        };
        if pending
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return false;
        }
        pending.claimed = true;
        true
    }

    pub(crate) fn release_pending_approval_claim(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        let pending = self.pending_approvals.get_mut(key)?;
        pending.claimed = false;
        if pending.is_expired_and_unclaimed() {
            self.pending_approvals.remove(key).map(|pending| pending.tx)
        } else {
            None
        }
    }

    pub(crate) fn remove_pending_approval(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        if self.pending_approvals.get(key)?.is_expired_and_unclaimed() {
            return None;
        }
        self.pending_approvals.remove(key).map(|pending| pending.tx)
    }

    pub(crate) fn remove_pending_approval_if_same(
        &mut self,
        key: &str,
        identity: &Arc<()>,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        let matches = self
            .pending_approvals
            .get(key)
            .is_some_and(|pending| Arc::ptr_eq(&pending.identity, identity) && !pending.claimed);
        if matches {
            self.pending_approvals.remove(key).map(|pending| pending.tx)
        } else {
            None
        }
    }

    pub(crate) fn clear_pending_waiters(&mut self) {
        self.pending_approvals.clear();
        self.pending_request_permissions.clear();
        self.pending_user_input.clear();
        self.pending_elicitations.clear();
        self.mcp_tool_approval_metadata.clear();
        self.pending_dynamic_tools.clear();
    }

    pub(crate) fn insert_pending_request_permissions(
        &mut self,
        key: String,
        pending_request_permissions: PendingRequestPermissions,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions
            .insert(key, pending_request_permissions)
    }

    pub(crate) fn remove_pending_request_permissions(
        &mut self,
        key: &str,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions.remove(key)
    }

    pub(crate) fn insert_pending_user_input(
        &mut self,
        key: String,
        tx: oneshot::Sender<RequestUserInputResponse>,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.insert(key, tx)
    }

    pub(crate) fn remove_pending_user_input(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.remove(key)
    }

    pub(crate) fn insert_pending_elicitation(
        &mut self,
        server_name: String,
        request_id: RequestId,
        tx: oneshot::Sender<ElicitationResponse>,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .insert((server_name, request_id), tx)
    }

    pub(crate) fn remove_pending_elicitation(
        &mut self,
        server_name: &str,
        request_id: &RequestId,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .remove(&(server_name.to_string(), request_id.clone()))
    }

    pub(crate) fn insert_mcp_tool_approval_metadata(
        &mut self,
        call_id: String,
        invocation: Option<McpInvocation>,
        metadata: McpToolApprovalMetadata,
    ) {
        self.mcp_tool_approval_metadata
            .insert(call_id, (invocation, metadata));
    }

    pub(crate) fn mcp_tool_approval_metadata(
        &self,
        call_id: &str,
    ) -> Option<(Option<McpInvocation>, McpToolApprovalMetadata)> {
        self.mcp_tool_approval_metadata.get(call_id).cloned()
    }

    pub(crate) fn insert_pending_dynamic_tool(
        &mut self,
        key: String,
        tx: oneshot::Sender<DynamicToolResponse>,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.insert(key, tx)
    }

    pub(crate) fn remove_pending_dynamic_tool(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.remove(key)
    }

    pub(crate) fn prepend_pending_input(&mut self, input: Vec<TurnInput>) {
        if input.is_empty() {
            return;
        }

        self.pending_input.append_to_front(input);
    }

    pub(crate) fn pending_input(&self) -> &[TurnInput] {
        self.pending_input.as_slice()
    }

    pub(crate) fn take_pending_input(&mut self) -> Vec<TurnInput> {
        if self.pending_input.is_empty() {
            Vec::with_capacity(0)
        } else {
            self.pending_input.take()
        }
    }

    pub(crate) fn accept_mailbox_delivery_for_current_turn(&mut self) {
        self.set_mailbox_delivery_phase(MailboxDeliveryPhase::CurrentTurn);
    }

    pub(crate) fn accepts_mailbox_delivery_for_current_turn(&self) -> bool {
        self.mailbox_delivery_phase == MailboxDeliveryPhase::CurrentTurn
    }

    pub(crate) fn set_mailbox_delivery_phase(&mut self, phase: MailboxDeliveryPhase) {
        self.mailbox_delivery_phase = phase;
    }

    pub(crate) fn record_granted_permissions(
        &mut self,
        environment_id: &str,
        permissions: AdditionalPermissionProfile,
    ) {
        let granted_permissions = merge_permission_profiles(
            self.granted_permissions_by_environment_id
                .get(environment_id),
            Some(&permissions),
        );
        if let Some(granted_permissions) = granted_permissions {
            self.granted_permissions_by_environment_id
                .insert(environment_id.to_string(), granted_permissions);
        }
    }

    pub(crate) fn granted_permissions(
        &self,
        environment_id: &str,
    ) -> Option<AdditionalPermissionProfile> {
        self.granted_permissions_by_environment_id
            .get(environment_id)
            .cloned()
    }

    pub(crate) fn enable_strict_auto_review(&mut self) {
        self.strict_auto_review_enabled = true;
    }

    pub(crate) fn strict_auto_review_enabled(&self) -> bool {
        self.strict_auto_review_enabled
    }
}

impl PendingApproval {
    fn is_expired_and_unclaimed(&self) -> bool {
        !self.claimed
            && self
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    }
}
