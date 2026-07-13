//! Generation-bound access to the goal extension's thread-scoped projection.

use super::GoalRestoreReason;
use super::GoalRuntimeHandle;

#[derive(Clone, Copy)]
pub(crate) enum InactiveGoalHistory {
    Preserve,
    Invalidate,
}

impl GoalRuntimeHandle {
    pub(crate) fn goal_revision(&self) -> u64 {
        self.inner.active_goal_objective.generation()
    }

    pub(crate) fn advance_goal_revision(&self) -> Result<u64, String> {
        self.inner
            .active_goal_objective
            .advance_generation()
            .map_err(str::to_owned)
    }

    pub(crate) fn goal_revision_is(&self, expected: u64) -> bool {
        self.goal_revision() == expected
    }

    pub(crate) fn clear_active_goal_objective(&self) {
        self.inner
            .active_goal_objective
            .clear_if_current(self.goal_revision());
    }

    pub(crate) fn project_active_goal_objective_at_revision(
        &self,
        expected_revision: u64,
        objective: String,
    ) -> bool {
        self.inner
            .active_goal_objective
            .project_if_current(expected_revision, objective)
    }

    pub(crate) fn clear_active_goal_objective_at_revision(
        &self,
        expected_revision: u64,
        _inactive_history: InactiveGoalHistory,
    ) -> bool {
        self.inner
            .active_goal_objective
            .clear_if_current(expected_revision)
    }
}

impl GoalRuntimeHandle {
    pub async fn restore_after_start(&self) -> Result<(), String> {
        self.restore_persisted_goal(GoalRestoreReason::ThreadStart)
            .await
    }

    pub async fn restore_after_resume(&self) -> Result<(), String> {
        self.restore_persisted_goal(GoalRestoreReason::ThreadResume)
            .await
    }

    async fn restore_persisted_goal(&self, reason: GoalRestoreReason) -> Result<(), String> {
        let revision = self.goal_revision();
        if !self.is_enabled() {
            self.clear_active_goal_objective_at_revision(revision, InactiveGoalHistory::Invalidate);
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }

        let _goal_state_permit = self.goal_state_permit().await?;
        let revision = self.goal_revision();
        let goal = match self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
        {
            Ok(goal) => goal,
            Err(err) => {
                self.clear_active_goal_objective_at_revision(
                    revision,
                    InactiveGoalHistory::Invalidate,
                );
                self.inner.accounting_state.clear_active_goal();
                return Err(err.to_string());
            }
        };
        match goal {
            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {
                if !self.project_active_goal_objective_at_revision(revision, goal.objective.clone())
                {
                    self.inner.accounting_state.clear_active_goal();
                    return Ok(());
                }
                self.inner
                    .accounting_state
                    .mark_idle_goal_active(goal.goal_id);
                if !self.is_enabled() {
                    self.inner.accounting_state.clear_active_goal();
                    self.clear_active_goal_objective();
                    return Ok(());
                }
                if matches!(reason, GoalRestoreReason::ThreadResume) {
                    self.inner.metrics.record_resumed();
                }
            }
            Some(_) | None => {
                self.clear_active_goal_objective_at_revision(
                    revision,
                    InactiveGoalHistory::Invalidate,
                );
                self.inner.accounting_state.clear_active_goal();
            }
        }
        Ok(())
    }
}
