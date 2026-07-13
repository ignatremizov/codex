//! Revision-guarded projection of the active goal objective.

use super::GoalRuntimeHandle;

#[derive(Clone, Copy)]
pub(crate) enum InactiveGoalHistory {
    Preserve,
    Invalidate,
}

impl GoalRuntimeHandle {
    pub(crate) fn clear_active_goal_objective(&self) {
        self.inner.active_goal_objective.replace(None);
    }

    pub(crate) fn project_active_goal_objective_at_revision(
        &self,
        expected_revision: u64,
        objective: String,
    ) -> bool {
        if !self.is_enabled() || !self.goal_revision_is(expected_revision) {
            return false;
        }
        self.inner.active_goal_objective.replace(Some(objective));
        self.is_enabled() && self.goal_revision_is(expected_revision)
    }

    pub(crate) fn clear_active_goal_objective_at_revision(
        &self,
        expected_revision: u64,
        _inactive_history: InactiveGoalHistory,
    ) -> bool {
        if !self.goal_revision_is(expected_revision) {
            return false;
        }
        self.inner.active_goal_objective.replace(None);
        self.goal_revision_is(expected_revision)
    }
}
