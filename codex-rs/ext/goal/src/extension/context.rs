use super::GoalExtension;
use super::goal_runtime_handle;
use crate::runtime::InactiveGoalHistory;
use crate::steering::continuation_steering_item;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::PostCompactionContextContribution;

impl<C> ContextContributor for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute_post_compaction_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, PostCompactionContextContribution> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(thread_store) else {
                return PostCompactionContextContribution::default();
            };
            let Ok(goal_state_permit) = runtime.goal_state_permit().await else {
                return PostCompactionContextContribution::default();
            };
            if !runtime.is_enabled() {
                return PostCompactionContextContribution::default();
            }
            let revision = runtime.goal_revision();
            let goal = match self
                .state_dbs
                .thread_goals()
                .get_thread_goal(runtime.thread_id())
                .await
            {
                Ok(Some(goal)) if goal.status == codex_state::ThreadGoalStatus::Active => goal,
                Ok(_) => {
                    runtime.clear_active_goal_objective_at_revision(
                        revision,
                        InactiveGoalHistory::Invalidate,
                    );
                    return PostCompactionContextContribution::default();
                }
                Err(err) => {
                    tracing::warn!(
                        thread_id = %runtime.thread_id(),
                        "failed to reconstruct active goal after compaction: {err}"
                    );
                    runtime.clear_active_goal_objective_at_revision(
                        revision,
                        InactiveGoalHistory::Invalidate,
                    );
                    return PostCompactionContextContribution::default();
                }
            };
            let update_plan_enabled = match self.thread_manager.upgrade() {
                Some(thread_manager) => {
                    match thread_manager.get_thread(runtime.thread_id()).await {
                        Ok(thread) => thread.config().await.update_plan_enabled,
                        Err(_) => true,
                    }
                }
                None => true,
            };
            PostCompactionContextContribution::with_lease_and_validation(
                vec![continuation_steering_item(
                    &crate::tool::protocol_goal_from_state(goal),
                    update_plan_enabled,
                )],
                goal_state_permit,
                move || runtime.is_enabled() && runtime.goal_revision_is(revision),
            )
        })
    }
}
