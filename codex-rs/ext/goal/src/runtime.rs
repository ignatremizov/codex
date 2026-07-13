use std::sync::Arc;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_core::TurnStartOptions;
use codex_extension_api::ActiveGoalObjective;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use crate::accounting::BudgetLimitedGoalDisposition;
use crate::accounting::GoalAccountingState;
use crate::analytics::GoalAnalytics;
use crate::analytics::GoalEventAttribution;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::tool::protocol_goal_from_state;

mod accounting;
mod continuation;
mod external_effects;
mod objective_projection;

pub(crate) use objective_projection::InactiveGoalHistory;

#[derive(Clone)]
pub struct GoalRuntimeHandle {
    inner: Arc<GoalRuntimeInner>,
}

pub(crate) struct GoalRuntimeConfig {
    pub(crate) analytics: GoalAnalytics,
    pub(crate) enabled: bool,
    pub(crate) tools_available_for_thread: bool,
    pub(crate) tools_visible_for_thread: bool,
    pub(crate) root_accounting_state: Option<Arc<GoalAccountingState>>,
}

pub(crate) enum ActiveGoalStopReason {
    TurnError,
    UsageLimit,
    ExecutionUnavailable { expected_goal_id: String },
    EmptyResponse,
}

#[derive(Clone, Copy)]
enum GoalRestoreReason {
    ThreadStart,
    ThreadResume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalObjectiveProjectionEffect {
    Preserve,
    Activated,
    Invalidated,
    DeferredUntilNextTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalContinuationEffect {
    None,
    StartIfIdle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExternalGoalSetRuntimeEffects {
    pub(crate) objective_projection: GoalObjectiveProjectionEffect,
    pub(crate) continuation: GoalContinuationEffect,
}

struct GoalRuntimeInner {
    thread_id: ThreadId,
    state_dbs: Arc<codex_state::StateRuntime>,
    analytics: GoalAnalytics,
    event_emitter: GoalEventEmitter,
    metrics: GoalMetrics,
    thread_manager: Weak<ThreadManager>,
    accounting_state: Arc<GoalAccountingState>,
    root_accounting_state: Option<Arc<GoalAccountingState>>,
    active_goal_objective: Arc<ActiveGoalObjective>,
    tools_available_for_thread: bool,
    tools_visible_for_thread: bool,
    goal_state_lock: Arc<Semaphore>,
}

pub(crate) struct AccountedGoalProgress {
    pub(crate) goal: ThreadGoal,
    pub(crate) goal_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviousGoalSnapshot {
    pub goal_id: String,
    pub status: codex_state::ThreadGoalStatus,
    pub objective: String,
}

impl From<&codex_state::ThreadGoal> for PreviousGoalSnapshot {
    fn from(goal: &codex_state::ThreadGoal) -> Self {
        Self {
            goal_id: goal.goal_id.clone(),
            status: goal.status,
            objective: goal.objective.clone(),
        }
    }
}

impl std::fmt::Debug for GoalRuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalRuntimeHandle").finish_non_exhaustive()
    }
}

impl GoalRuntimeHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        thread_id: ThreadId,
        state_dbs: Arc<codex_state::StateRuntime>,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
        thread_manager: Weak<ThreadManager>,
        accounting_state: Arc<GoalAccountingState>,
        active_goal_objective: Arc<ActiveGoalObjective>,
        config: GoalRuntimeConfig,
    ) -> Self {
        let runtime = Self {
            inner: Arc::new(GoalRuntimeInner {
                thread_id,
                state_dbs,
                analytics: config.analytics,
                event_emitter,
                metrics,
                thread_manager,
                accounting_state,
                root_accounting_state: config.root_accounting_state,
                active_goal_objective,
                tools_available_for_thread: config.tools_available_for_thread,
                tools_visible_for_thread: config.tools_visible_for_thread,
                goal_state_lock: Arc::new(Semaphore::new(/*permits*/ 1)),
            }),
        };
        runtime.set_enabled(config.enabled);
        runtime
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        if let Err(error) = self.inner.active_goal_objective.set_enabled(enabled) {
            tracing::error!(thread_id = %self.thread_id(), "goal projection disabled: {error}");
        }
        if !self.is_enabled() {
            self.inner.accounting_state.clear_active_goal();
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.active_goal_objective.enabled()
    }

    pub(crate) fn tools_visible(&self) -> bool {
        self.is_enabled() && self.inner.tools_visible_for_thread
    }

    pub(crate) fn tools_available(&self) -> bool {
        self.is_enabled() && self.inner.tools_available_for_thread
    }

    pub(crate) fn thread_id(&self) -> ThreadId {
        self.inner.thread_id
    }

    pub(crate) fn accounting_state(&self) -> Arc<GoalAccountingState> {
        Arc::clone(&self.inner.accounting_state)
    }

    pub(crate) fn root_accounting_state(&self) -> Option<Arc<GoalAccountingState>> {
        self.inner.root_accounting_state.clone()
    }

    pub(crate) async fn clear_pending_turn_start_options(&self) {
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            return;
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            return;
        };
        thread.thread_extension_data().remove::<TurnStartOptions>();
    }

    pub(crate) async fn goal_state_permit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.inner
            .goal_state_lock
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| err.to_string())
    }

    pub async fn prepare_external_goal_mutation(&self) -> Result<(), String> {
        let _goal_state_permit = self.goal_state_permit().await?;
        self.prepare_external_goal_mutation_locked().await
    }

    pub(crate) async fn prepare_external_goal_mutation_locked(&self) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }
        // Invalidate the old turn before the persisted objective/status changes.
        self.inner.accounting_state.reset_empty_responses();

        if let Some(turn_id) = self.inner.accounting_state.current_turn_id() {
            self.account_active_goal_progress_locked(
                turn_id.as_str(),
                &format!("{turn_id}:external-goal-mutation"),
                codex_state::GoalAccountingMode::ActiveOnly,
                BudgetLimitedGoalDisposition::ClearActive,
            )
            .await?;
            return Ok(());
        }

        self.account_idle_goal_progress_locked(
            &format!("{}:external-goal-mutation", self.inner.thread_id),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;
        Ok(())
    }

    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {
        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)
            .await
    }

    /// Accounts the ending turn and stops its active goal after an error or repeated empty output.
    pub(crate) async fn stop_active_goal_for_turn(
        &self,
        turn_id: &str,
        reason: ActiveGoalStopReason,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        // Hold this through accounting and the status update so external goal
        // mutations and idle continuation cannot interleave between them.
        let _goal_state_permit = self.goal_state_permit().await?;
        let Some(accounting_goal_id) = self
            .inner
            .accounting_state
            .current_active_goal_id_for_turn(turn_id)
        else {
            return Ok(());
        };
        if let ActiveGoalStopReason::ExecutionUnavailable { expected_goal_id } = &reason
            && accounting_goal_id != *expected_goal_id
        {
            return Ok(());
        }

        let (event_name, status, expected_goal_id) = match reason {
            ActiveGoalStopReason::TurnError => {
                ("turn-error", codex_state::ThreadGoalStatus::Blocked, None)
            }
            ActiveGoalStopReason::UsageLimit => (
                "usage-limit",
                codex_state::ThreadGoalStatus::UsageLimited,
                None,
            ),
            ActiveGoalStopReason::EmptyResponse => {
                let Some(expected_goal_id) =
                    self.inner.accounting_state.empty_response_goal(turn_id)
                else {
                    return Ok(());
                };
                if accounting_goal_id != expected_goal_id {
                    return Ok(());
                }
                (
                    "empty-response",
                    codex_state::ThreadGoalStatus::Blocked,
                    Some(expected_goal_id),
                )
            }
            ActiveGoalStopReason::ExecutionUnavailable { expected_goal_id } => (
                "execution-unavailable",
                codex_state::ThreadGoalStatus::Blocked,
                Some(expected_goal_id),
            ),
        };
        self.account_active_goal_progress_locked(
            turn_id,
            &format!("{turn_id}:{event_name}-progress"),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;

        let Some(active_goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            self.inner.accounting_state.clear_active_goal();
            self.clear_active_goal_objective();
            return Ok(());
        };
        if expected_goal_id
            .as_ref()
            .is_some_and(|expected_goal_id| active_goal.goal_id != *expected_goal_id)
        {
            return Ok(());
        }
        let can_stop = active_goal.status == codex_state::ThreadGoalStatus::Active
            || (active_goal.status == codex_state::ThreadGoalStatus::BudgetLimited
                && status == codex_state::ThreadGoalStatus::UsageLimited);
        if !can_stop {
            self.inner.accounting_state.clear_active_goal();
            self.clear_active_goal_objective();
            return Ok(());
        }
        let previous_status = Some(active_goal.status);
        let Some(goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .update_thread_goal(
                self.thread_id(),
                codex_state::GoalUpdate {
                    objective: None,
                    status: Some(status),
                    token_budget: None,
                    expected_goal_id: Some(active_goal.goal_id),
                },
            )
            .await
            .map_err(|err| err.to_string())?
        else {
            self.inner.accounting_state.clear_active_goal();
            self.clear_active_goal_objective();
            return Ok(());
        };
        let revision = self.advance_goal_revision()?;
        self.inner
            .metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.inner.analytics.status_changed(
            &goal,
            previous_status,
            GoalEventAttribution::Turn(turn_id),
        );
        self.inner.accounting_state.clear_active_goal();
        self.clear_active_goal_objective_at_revision(revision, InactiveGoalHistory::Invalidate);
        let goal = protocol_goal_from_state(goal);
        self.inner.event_emitter.thread_goal_updated(
            format!("{turn_id}:{event_name}"),
            Some(turn_id.to_string()),
            goal,
        );
        Ok(())
    }

    pub(crate) async fn inject_active_turn_steering(&self, item: ResponseItem) {
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal steering because thread manager is unavailable");
            return;
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal steering because live thread is unavailable");
            return;
        };
        if thread.inject_if_running(vec![item]).await.is_err() {
            tracing::debug!("skipping goal steering because no turn is active");
        }
    }
}
