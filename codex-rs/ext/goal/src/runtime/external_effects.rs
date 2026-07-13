use super::ExternalGoalSetRuntimeEffects;
use super::GoalContinuationEffect;
use super::GoalObjectiveProjectionEffect;
use super::GoalRuntimeHandle;
use super::InactiveGoalHistory;
use super::PreviousGoalSnapshot;
use crate::analytics::GoalEventAttribution;
use crate::steering::objective_updated_steering_item;
use crate::tool::protocol_goal_from_state;

impl GoalRuntimeHandle {
    pub(crate) async fn apply_external_goal_set_locked(
        &self,
        goal: codex_state::ThreadGoal,
        previous_goal: Option<PreviousGoalSnapshot>,
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
            .get_thread_goal(self.thread_id())
            .await;
        let current_goal = match verification {
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
        if !same_goal_mutation(&current_goal, &goal) {
            self.fail_closed_external_projection(effects, expected_revision)
                .await;
            return Ok(false);
        }
        let goal = current_goal;
        if !self.is_enabled() {
            if !matches!(
                effects.objective_projection,
                GoalObjectiveProjectionEffect::Invalidated
            ) {
                self.clear_active_goal_objective_at_revision(
                    expected_revision,
                    InactiveGoalHistory::Invalidate,
                );
            }
            return Ok(false);
        }
        match effects.objective_projection {
            GoalObjectiveProjectionEffect::Preserve
            | GoalObjectiveProjectionEffect::Invalidated => {}
            GoalObjectiveProjectionEffect::Activated => {
                if goal.status != codex_state::ThreadGoalStatus::Active
                    || !self.project_active_goal_objective_at_revision(
                        expected_revision,
                        goal.objective.clone(),
                    )
                {
                    self.fail_closed_external_projection(effects, expected_revision)
                        .await;
                    return Ok(false);
                }
            }
            GoalObjectiveProjectionEffect::DeferredUntilNextTurn => {
                if goal.status == codex_state::ThreadGoalStatus::Active
                    && self.inner.accounting_state.current_turn_id().is_none()
                    && !self.project_active_goal_objective_at_revision(
                        expected_revision,
                        goal.objective.clone(),
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

        self.inner.accounting_state.reset_empty_responses();
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
                    self.clear_active_goal_objective();
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
        match effects.objective_projection {
            GoalObjectiveProjectionEffect::Activated => {
                self.clear_active_goal_objective_at_revision(
                    expected_revision,
                    InactiveGoalHistory::Invalidate,
                );
            }
            GoalObjectiveProjectionEffect::Invalidated
            | GoalObjectiveProjectionEffect::Preserve
            | GoalObjectiveProjectionEffect::DeferredUntilNextTurn => {}
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
}

fn same_goal_mutation(left: &codex_state::ThreadGoal, right: &codex_state::ThreadGoal) -> bool {
    left.thread_id == right.thread_id
        && left.goal_id == right.goal_id
        && left.objective == right.objective
        && left.status == right.status
        && left.token_budget == right.token_budget
        && left.created_at == right.created_at
}
