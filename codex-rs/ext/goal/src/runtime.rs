use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_core::ThreadManager;
use codex_extension_api::GoalSkillActivations;
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
use crate::steering::continuation_steering_item;
use crate::steering::objective_updated_steering_item;
use crate::tool::protocol_goal_from_state;

mod skill_projection;

pub(crate) use skill_projection::InactiveGoalHistory;

#[derive(Clone)]
pub struct GoalRuntimeHandle {
    inner: Arc<GoalRuntimeInner>,
}

pub(crate) struct GoalRuntimeConfig {
    pub(crate) analytics: GoalAnalytics,
    pub(crate) enabled: bool,
    pub(crate) tools_available_for_thread: bool,
}

pub(crate) enum ActiveGoalStopReason {
    TurnError,
    UsageLimit,
}

#[derive(Clone, Copy)]
enum GoalRestoreReason {
    ThreadStart,
    ThreadResume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalSkillProjectionEffect {
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
    pub(crate) skill_projection: GoalSkillProjectionEffect,
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
    goal_skill_activations: Arc<GoalSkillActivations>,
    enabled: AtomicBool,
    tools_available_for_thread: bool,
    goal_state_lock: Arc<Semaphore>,
    goal_revision: Mutex<u64>,
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
        goal_skill_activations: Arc<GoalSkillActivations>,
        config: GoalRuntimeConfig,
    ) -> Self {
        Self {
            inner: Arc::new(GoalRuntimeInner {
                thread_id,
                state_dbs,
                analytics: config.analytics,
                event_emitter,
                metrics,
                thread_manager,
                accounting_state,
                goal_skill_activations,
                enabled: AtomicBool::new(config.enabled),
                tools_available_for_thread: config.tools_available_for_thread,
                goal_state_lock: Arc::new(Semaphore::new(/*permits*/ 1)),
                goal_revision: Mutex::new(0),
            }),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let previous = self.inner.enabled.swap(enabled, Ordering::AcqRel);
        if previous != enabled
            && let Err(err) = self.advance_goal_revision()
        {
            tracing::error!(
                thread_id = %self.thread_id(),
                "failed to advance goal revision after configuration change: {err}"
            );
        }
        if !enabled {
            self.clear_goal_skill_activations();
            self.inner.accounting_state.clear_active_goal();
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn tools_visible(&self) -> bool {
        self.is_enabled() && self.inner.tools_available_for_thread
    }

    pub(crate) fn thread_id(&self) -> ThreadId {
        self.inner.thread_id
    }

    pub(crate) fn accounting_state(&self) -> Arc<GoalAccountingState> {
        Arc::clone(&self.inner.accounting_state)
    }

    pub(crate) async fn goal_state_permit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.inner
            .goal_state_lock
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| err.to_string())
    }

    pub(crate) fn goal_revision(&self) -> u64 {
        *self
            .inner
            .goal_revision
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn advance_goal_revision(&self) -> Result<u64, String> {
        let mut current = self
            .inner
            .goal_revision
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let revision = current
            .checked_add(1)
            .ok_or_else(|| "goal runtime revision is exhausted".to_string())?;
        // Keep revision publication and activation ownership rebinding serialized. Callers that
        // observed the previous revision can only mutate the bridge before this critical section
        // or fail its revision-bound compare-and-swap after it.
        *current = revision;
        Ok(revision)
    }

    pub(crate) fn goal_revision_is(&self, expected: u64) -> bool {
        self.goal_revision() == expected
    }

    pub async fn prepare_external_goal_mutation(&self) -> Result<(), String> {
        let _goal_state_permit = self.goal_state_permit().await?;
        self.prepare_external_goal_mutation_locked().await
    }

    pub(crate) async fn prepare_external_goal_mutation_locked(&self) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

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

    pub(crate) async fn apply_external_goal_set_locked(
        &self,
        goal: codex_state::ThreadGoal,
        previous_goal: Option<PreviousGoalSnapshot>,
        skill_selections: Vec<codex_protocol::protocol::GoalSkillSelection>,
        effects: ExternalGoalSetRuntimeEffects,
        expected_revision: u64,
    ) -> Result<bool, String> {
        if !self.goal_revision_is(expected_revision) {
            self.fail_closed_external_projection(effects, expected_revision)
                .await;
            return Ok(false);
        }
        let verification = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal_with_skill_selections(self.thread_id())
            .await;
        let (current_goal, current_skill_selections) = match verification {
            Ok(Some(current)) => current,
            Ok(None) => {
                self.fail_closed_external_projection(effects, expected_revision)
                    .await;
                return Ok(false);
            }
            Err(err) => {
                self.fail_closed_external_projection(effects, expected_revision)
                    .await;
                return Err(err.to_string());
            }
        };
        if !same_goal_mutation(&current_goal, &goal) || current_skill_selections != skill_selections
        {
            self.fail_closed_external_projection(effects, expected_revision)
                .await;
            return Ok(false);
        }
        let goal = current_goal;
        if !self.is_enabled() {
            if !matches!(
                effects.skill_projection,
                GoalSkillProjectionEffect::Invalidated
            ) {
                self.clear_goal_skill_activations_at_revision(
                    expected_revision,
                    InactiveGoalHistory::Invalidate,
                );
            }
            return Ok(false);
        }
        match effects.skill_projection {
            GoalSkillProjectionEffect::Preserve | GoalSkillProjectionEffect::Invalidated => {}
            GoalSkillProjectionEffect::Activated => {
                if goal.status != codex_state::ThreadGoalStatus::Active
                    || !self.activate_goal_skill_selections_at_revision(
                        expected_revision,
                        goal.goal_id.clone(),
                        skill_selections.clone(),
                    )
                {
                    self.fail_closed_external_projection(effects, expected_revision)
                        .await;
                    return Ok(false);
                }
            }
            GoalSkillProjectionEffect::DeferredUntilNextTurn => {
                if goal.status == codex_state::ThreadGoalStatus::Active
                    && self.inner.accounting_state.current_turn_id().is_none()
                    && !self.activate_goal_skill_selections_at_revision(
                        expected_revision,
                        goal.goal_id.clone(),
                        skill_selections.clone(),
                    )
                {
                    self.fail_closed_external_projection(effects, expected_revision)
                        .await;
                    return Ok(false);
                }
            }
        }
        if !self.goal_revision_is(expected_revision) {
            return Ok(false);
        }

        let replaced_existing_goal = previous_goal
            .as_ref()
            .is_some_and(|previous_goal| previous_goal.goal_id != goal.goal_id);
        if previous_goal.is_none() || replaced_existing_goal {
            self.inner.metrics.record_created();
            self.inner
                .analytics
                .created(&goal, GoalEventAttribution::NoTurn);
        }
        let previous_status = previous_goal
            .as_ref()
            .and_then(|previous_goal| (!replaced_existing_goal).then_some(previous_goal.status));
        self.inner
            .metrics
            .record_resumed_if_status_changed(previous_status, goal.status);
        self.inner
            .metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.inner
            .analytics
            .status_changed(&goal, previous_status, GoalEventAttribution::NoTurn);
        let objective_changed = previous_goal.as_ref().is_some_and(|previous_goal| {
            !replaced_existing_goal && previous_goal.objective != goal.objective
        });
        let continue_if_idle = goal.status == codex_state::ThreadGoalStatus::Active
            && self.inner.accounting_state.current_turn_id().is_none()
            && matches!(effects.continuation, GoalContinuationEffect::StartIfIdle);
        match goal.status {
            codex_state::ThreadGoalStatus::Active => {
                if self.inner.accounting_state.current_turn_id().is_some() {
                    let _ = self
                        .inner
                        .accounting_state
                        .mark_current_turn_goal_active(goal.goal_id.clone());
                } else {
                    self.inner
                        .accounting_state
                        .mark_idle_goal_active(goal.goal_id.clone());
                }
                if !self.is_enabled() {
                    self.inner.accounting_state.clear_active_goal();
                    self.clear_goal_skill_activations();
                    return Ok(false);
                }
                if objective_changed {
                    let item = objective_updated_steering_item(&protocol_goal_from_state(goal));
                    self.inject_active_turn_steering(item).await;
                }
            }
            codex_state::ThreadGoalStatus::BudgetLimited => {
                if self.inner.accounting_state.current_turn_id().is_none() {
                    self.inner.accounting_state.clear_active_goal();
                }
            }
            codex_state::ThreadGoalStatus::Paused
            | codex_state::ThreadGoalStatus::Blocked
            | codex_state::ThreadGoalStatus::UsageLimited
            | codex_state::ThreadGoalStatus::Complete => {
                self.inner.accounting_state.clear_active_goal();
            }
        }
        Ok(continue_if_idle)
    }

    pub(crate) async fn fail_closed_external_projection(
        &self,
        effects: ExternalGoalSetRuntimeEffects,
        expected_revision: u64,
    ) {
        match effects.skill_projection {
            GoalSkillProjectionEffect::Activated => {
                self.clear_goal_skill_activations_at_revision(
                    expected_revision,
                    InactiveGoalHistory::Invalidate,
                );
            }
            GoalSkillProjectionEffect::Invalidated
            | GoalSkillProjectionEffect::Preserve
            | GoalSkillProjectionEffect::DeferredUntilNextTurn => {}
        }
    }

    pub(crate) async fn apply_external_goal_clear_locked(
        &self,
        goal: Option<codex_state::ThreadGoal>,
        expected_revision: u64,
    ) -> Result<(), String> {
        if !self.goal_revision_is(expected_revision) {
            return Ok(());
        }
        match self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
        {
            Ok(None) => {}
            Ok(Some(_)) => {
                return Ok(());
            }
            Err(err) => {
                return Err(err.to_string());
            }
        }
        self.inner.accounting_state.clear_active_goal();
        if self.is_enabled()
            && self.goal_revision_is(expected_revision)
            && let Some(goal) = goal
        {
            self.inner.analytics.cleared(&goal);
        }
        Ok(())
    }

    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {
        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)
            .await
    }

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
        if !self
            .inner
            .accounting_state
            .turn_is_current_active_goal(turn_id)
        {
            return Ok(());
        }

        let (event_name, status) = match reason {
            ActiveGoalStopReason::TurnError => {
                ("turn-error", codex_state::ThreadGoalStatus::Blocked)
            }
            ActiveGoalStopReason::UsageLimit => {
                ("usage-limit", codex_state::ThreadGoalStatus::UsageLimited)
            }
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
            self.clear_goal_skill_activations();
            return Ok(());
        };
        let can_stop = active_goal.status == codex_state::ThreadGoalStatus::Active
            || (active_goal.status == codex_state::ThreadGoalStatus::BudgetLimited
                && status == codex_state::ThreadGoalStatus::UsageLimited);
        if !can_stop {
            self.inner.accounting_state.clear_active_goal();
            self.clear_goal_skill_activations();
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
            self.clear_goal_skill_activations();
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
        self.clear_goal_skill_activations_at_revision(revision, InactiveGoalHistory::Invalidate);
        let goal = protocol_goal_from_state(goal);
        self.inner.event_emitter.thread_goal_updated(
            format!("{turn_id}:{event_name}"),
            Some(turn_id.to_string()),
            goal,
        );
        Ok(())
    }

    pub async fn restore_after_start(&self) -> Result<(), String> {
        self.restore_persisted_goal(GoalRestoreReason::ThreadStart)
            .await
    }

    pub async fn restore_after_resume(&self) -> Result<(), String> {
        self.restore_persisted_goal(GoalRestoreReason::ThreadResume)
            .await
    }

    async fn restore_persisted_goal(&self, reason: GoalRestoreReason) -> Result<(), String> {
        if !self.is_enabled() {
            let revision = self.goal_revision();
            self.clear_goal_skill_activations_at_revision(
                revision,
                InactiveGoalHistory::Invalidate,
            );
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }

        let _goal_state_permit = self.goal_state_permit().await?;
        let revision = self.goal_revision();
        let goal = match self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal_with_skill_selections(self.thread_id())
            .await
        {
            Ok(goal) => goal,
            Err(err) => {
                self.clear_goal_skill_activations_at_revision(
                    revision,
                    InactiveGoalHistory::Invalidate,
                );
                self.inner.accounting_state.clear_active_goal();
                return Err(err.to_string());
            }
        };
        match goal {
            Some((goal, skill_selections))
                if goal.status == codex_state::ThreadGoalStatus::Active =>
            {
                if !self.activate_goal_skill_selections_at_revision(
                    revision,
                    goal.goal_id.clone(),
                    skill_selections,
                ) {
                    self.inner.accounting_state.clear_active_goal();
                    return Ok(());
                }
                self.inner
                    .accounting_state
                    .mark_idle_goal_active(goal.goal_id);
                if !self.is_enabled() {
                    self.inner.accounting_state.clear_active_goal();
                    self.clear_goal_skill_activations();
                    return Ok(());
                }
                if matches!(reason, GoalRestoreReason::ThreadResume) {
                    self.inner.metrics.record_resumed();
                }
            }
            Some(_) | None => {
                self.clear_goal_skill_activations_at_revision(
                    revision,
                    InactiveGoalHistory::Invalidate,
                );
                self.inner.accounting_state.clear_active_goal();
            }
        }
        Ok(())
    }

    pub(crate) async fn continue_if_idle(&self) -> Result<(), String> {
        if !self.tools_visible() {
            self.inner.accounting_state.clear_active_goal();
            self.clear_goal_skill_activations();
            return Ok(());
        }
        // Hold this through the read/start window so external set/clear cannot
        // change the goal after we read it but before the continuation launches.
        let _goal_state_permit = self.goal_state_permit().await?;
        let revision = self.goal_revision();

        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal continuation because thread manager is unavailable");
            return Ok(());
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal continuation because live thread is unavailable");
            return Ok(());
        };

        let Some((goal, skill_selections)) = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal_with_skill_selections(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            self.inner.accounting_state.clear_active_goal();
            self.clear_goal_skill_activations_at_revision(revision, InactiveGoalHistory::Preserve);
            return Ok(());
        };
        if goal.status != codex_state::ThreadGoalStatus::Active {
            self.inner.accounting_state.clear_active_goal();
            self.clear_goal_skill_activations_at_revision(revision, InactiveGoalHistory::Preserve);
            return Ok(());
        }
        if !self.activate_goal_skill_selections_at_revision(
            revision,
            goal.goal_id.clone(),
            skill_selections,
        ) {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        if !self.goal_revision_is(revision) {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        let item = continuation_steering_item(&protocol_goal_from_state(goal));

        if let Err(err) = thread
            .try_start_turn_if_idle_with_lease(vec![item], _goal_state_permit)
            .await
        {
            let reason = err.reason();
            tracing::debug!(
                ?reason,
                "skipping goal continuation because automatic idle work was rejected"
            );
        }

        let current_turn_is_goal_active = self
            .inner
            .accounting_state
            .current_turn_id()
            .is_some_and(|turn_id| {
                self.inner
                    .accounting_state
                    .turn_is_current_active_goal(turn_id.as_str())
            });
        if !current_turn_is_goal_active {
            self.inner.accounting_state.clear_active_goal();
        }
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

    pub(crate) async fn account_active_goal_progress(
        &self,
        turn_id: &str,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let _goal_state_permit = self.goal_state_permit().await?;
        self.account_active_goal_progress_locked(
            turn_id,
            event_id,
            mode,
            budget_limited_goal_disposition,
        )
        .await
    }

    async fn account_active_goal_progress_locked(
        &self,
        turn_id: &str,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let accounting = self.accounting_state();
        let _accounting_permit = accounting
            .progress_accounting_permit()
            .await
            .map_err(|err| err.to_string())?;
        let Some(snapshot) = accounting.progress_snapshot(turn_id) else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .inner
            .state_dbs
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id(),
                snapshot.time_delta_seconds,
                snapshot.token_delta,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                let goal_id = goal.goal_id.clone();
                if previous_status != Some(goal.status) {
                    let revision = self.advance_goal_revision()?;
                    if goal.status != codex_state::ThreadGoalStatus::Active {
                        self.clear_goal_skill_activations_at_revision(
                            revision,
                            InactiveGoalHistory::Invalidate,
                        );
                    }
                }
                self.inner
                    .metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.inner
                    .analytics
                    .usage_accounted(&goal, GoalEventAttribution::Turn(turn_id));
                self.inner.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::Turn(turn_id),
                );
                accounting.mark_progress_accounted_for_status(
                    turn_id,
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.inner.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    Some(turn_id.to_string()),
                    goal.clone(),
                );
                Some(AccountedGoalProgress { goal, goal_id })
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => None,
        })
    }

    async fn account_idle_goal_progress_locked(
        &self,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let accounting = self.accounting_state();
        let _accounting_permit = accounting
            .progress_accounting_permit()
            .await
            .map_err(|err| err.to_string())?;
        let Some(snapshot) = accounting.idle_progress_snapshot() else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .inner
            .state_dbs
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id(),
                snapshot.time_delta_seconds,
                /*token_delta*/ 0,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                let goal_id = goal.goal_id.clone();
                if previous_status != Some(goal.status) {
                    let revision = self.advance_goal_revision()?;
                    if goal.status != codex_state::ThreadGoalStatus::Active {
                        self.clear_goal_skill_activations_at_revision(
                            revision,
                            InactiveGoalHistory::Invalidate,
                        );
                    }
                }
                self.inner
                    .metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.inner
                    .analytics
                    .usage_accounted(&goal, GoalEventAttribution::NoTurn);
                self.inner.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::NoTurn,
                );
                accounting.mark_idle_progress_accounted_for_status(
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.inner.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    /*turn_id*/ None,
                    goal.clone(),
                );
                Some(AccountedGoalProgress { goal, goal_id })
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => {
                accounting.reset_idle_progress_baseline_and_clear_active_goal();
                None
            }
        })
    }

    async fn current_goal_status_for_metrics(
        &self,
        expected_goal_id: Option<&str>,
    ) -> Result<Option<codex_state::ThreadGoalStatus>, String> {
        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        Ok(goal.and_then(|goal| {
            expected_goal_id
                .is_none_or(|expected_goal_id| goal.goal_id == expected_goal_id)
                .then_some(goal.status)
        }))
    }
}

fn same_goal_mutation(left: &codex_state::ThreadGoal, right: &codex_state::ThreadGoal) -> bool {
    left.thread_id == right.thread_id
        && left.goal_id == right.goal_id
        && left.objective == right.objective
        && left.status == right.status
        && left.token_budget == right.token_budget
        && left.created_at == right.created_at
}
