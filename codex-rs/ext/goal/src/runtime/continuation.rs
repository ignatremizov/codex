use super::GoalRuntimeHandle;
use super::InactiveGoalHistory;
use crate::steering::continuation_steering_item;
use crate::tool::protocol_goal_from_state;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;

impl GoalRuntimeHandle {
    pub(crate) async fn continue_if_idle(&self) -> Result<(), String> {
        let revision = self.goal_revision();
        if !self.tools_available() {
            self.inner.accounting_state.clear_active_goal();
            self.clear_active_goal_objective_at_revision(revision, InactiveGoalHistory::Invalidate);
            return Ok(());
        }
        // Hold this through the read/start window so external set/clear cannot
        // change the goal after we read it but before the continuation launches.
        let goal_state_permit = self.goal_state_permit().await?;
        let revision = self.goal_revision();

        if self
            .inner
            .state_dbs
            .thread_goals()
            .has_thread_goal_continuation_deferral(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        {
            return Ok(());
        }

        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal continuation because thread manager is unavailable");
            return Ok(());
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal continuation because live thread is unavailable");
            return Ok(());
        };

        let goal = match self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
        {
            Ok(Some(goal)) => goal,
            Ok(None) => {
                self.inner.accounting_state.clear_active_goal();
                self.clear_active_goal_objective_at_revision(
                    revision,
                    InactiveGoalHistory::Preserve,
                );
                return Ok(());
            }
            Err(err) => {
                self.inner.accounting_state.clear_active_goal();
                self.clear_active_goal_objective_at_revision(
                    revision,
                    InactiveGoalHistory::Invalidate,
                );
                return Err(err.to_string());
            }
        };
        if goal.status != codex_state::ThreadGoalStatus::Active {
            self.inner.accounting_state.clear_active_goal();
            self.clear_active_goal_objective_at_revision(revision, InactiveGoalHistory::Preserve);
            return Ok(());
        }
        if !self.project_active_goal_objective_at_revision(revision, goal.objective.clone()) {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        if !self.goal_revision_is(revision) {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        let start_options = thread
            .thread_extension_data()
            .get::<TurnStartOptions>()
            .map(|options| options.as_ref().clone())
            .unwrap_or_default();
        let item = continuation_steering_item(
            &protocol_goal_from_state(goal),
            thread.config().await.update_plan_enabled,
        );

        match thread
            .start_turn_if_idle_with_lease(
                TurnInputRequest::new(TurnInput::ResponseItem(item)).on_start(TurnStartOptions {
                    turn_trigger: Some("goal".to_string()),
                    ..start_options
                }),
                goal_state_permit,
                |turn_id| {
                    self.inner
                        .accounting_state
                        .mark_goal_continuation(turn_id.to_string())
                },
            )
            .await
        {
            Ok(StartIfIdleSubmission::Started { .. }) => {}
            Ok(StartIfIdleSubmission::NotSubmitted { reason }) => {
                tracing::debug!(
                    ?reason,
                    "skipping goal continuation because automatic idle work was rejected"
                );
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    "skipping goal continuation because turn input submission failed"
                );
            }
        }

        let current_turn_is_goal_active = self
            .inner
            .accounting_state
            .current_turn_id()
            .is_some_and(|turn_id| {
                self.inner
                    .accounting_state
                    .current_active_goal_id_for_turn(turn_id.as_str())
                    .is_some()
            });
        if !current_turn_is_goal_active {
            self.inner
                .accounting_state
                .reset_idle_progress_baseline_and_clear_active_goal();
        }
        Ok(())
    }
}
