use super::GoalRuntimeHandle;

#[derive(Clone, Copy)]
pub(crate) enum InactiveGoalHistory {
    Preserve,
    Invalidate,
}

impl GoalRuntimeHandle {
    pub(crate) fn clear_goal_skill_activations(&self) {
        self.inner.goal_skill_activations.replace(Vec::new());
    }

    pub(crate) fn activate_goal_skill_selections_at_revision(
        &self,
        expected_revision: u64,
        _goal_id: impl Into<String>,
        selections: Vec<codex_protocol::protocol::GoalSkillSelection>,
    ) -> bool {
        if !self.is_enabled() || !self.goal_revision_is(expected_revision) {
            return false;
        }
        self.inner.goal_skill_activations.replace(selections);
        self.is_enabled() && self.goal_revision_is(expected_revision)
    }

    pub(crate) fn clear_goal_skill_activations_at_revision(
        &self,
        expected_revision: u64,
        _inactive_history: InactiveGoalHistory,
    ) -> bool {
        if !self.goal_revision_is(expected_revision) {
            return false;
        }
        self.inner.goal_skill_activations.replace(Vec::new());
        self.goal_revision_is(expected_revision)
    }
}
