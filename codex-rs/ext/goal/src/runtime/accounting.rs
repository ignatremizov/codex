use super::AccountedGoalProgress;
use super::GoalRuntimeHandle;
use super::InactiveGoalHistory;
use crate::accounting::BudgetLimitedGoalDisposition;
use crate::analytics::GoalEventAttribution;
use crate::tool::protocol_goal_from_state;

impl GoalRuntimeHandle {
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

    pub(super) async fn account_active_goal_progress_locked(
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
                        self.clear_active_goal_objective_at_revision(
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

    pub(super) async fn account_idle_goal_progress_locked(
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
                        self.clear_active_goal_objective_at_revision(
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
