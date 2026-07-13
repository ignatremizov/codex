use super::GoalClearOutcome;
use super::GoalObjectiveUpdate;
use super::GoalService;
use super::GoalServiceError;
use super::GoalSetOutcome;
use super::GoalSetRequest;
use super::GoalTokenBudgetUpdate;
use crate::runtime::ExternalGoalSetRuntimeEffects;
use crate::runtime::GoalContinuationEffect;
use crate::runtime::GoalObjectiveProjectionEffect;
use crate::runtime::GoalRuntimeHandle;
use crate::runtime::InactiveGoalHistory;
use crate::runtime::PreviousGoalSnapshot;
use crate::tool::fill_empty_thread_preview_if_possible;
use crate::tool::protocol_goal_from_state;
use crate::tool::state_status_from_protocol;
use crate::tool::validate_goal_budget;
use codex_protocol::ThreadId;
use codex_protocol::protocol::validate_thread_goal_objective;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;

impl GoalService {
    pub async fn set_thread_goal(
        &self,
        state_db: &codex_state::StateRuntime,
        request: GoalSetRequest<'_>,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let GoalSetRequest {
            thread_id,
            objective,
            status,
            token_budget,
            max_goal_token_budget,
        } = request;
        let status = status.map(state_status_from_protocol);
        let objective = match objective {
            GoalObjectiveUpdate::Keep => None,
            GoalObjectiveUpdate::Set(objective) => Some(objective.trim()),
        };
        let token_budget = match token_budget {
            GoalTokenBudgetUpdate::Keep => None,
            GoalTokenBudgetUpdate::Set(token_budget) => {
                Some(token_budget.or(max_goal_token_budget))
            }
        };
        if let Some(objective) = objective {
            validate_thread_goal_objective(objective).map_err(GoalServiceError::InvalidRequest)?;
        }
        if objective.is_some() || token_budget.is_some() {
            validate_goal_budget(token_budget.flatten(), max_goal_token_budget)
                .map_err(GoalServiceError::InvalidRequest)?;
        }

        let external_effect_permit = self
            .external_effect_lock(thread_id)
            .acquire_owned()
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?;
        let runtime = self.runtime_for_thread(thread_id);
        // Hold this through the prepare/write window so idle continuation cannot
        // launch from goal state that this external mutation is about to change.
        let _goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let mut existing = state_db
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .map_err(|err| {
                GoalServiceError::Internal(format!("failed to read thread goal: {err}"))
            })?;
        if let Some(goal) = existing.as_ref()
            && goal_set_request_is_noop(goal, objective, status, token_budget)
        {
            drop(_goal_state_permit);
            return Ok(unchanged_goal_set_outcome(
                goal.clone(),
                runtime,
                external_effect_permit,
            ));
        }

        if let Some(runtime) = runtime.as_ref()
            && let Err(err) = runtime.prepare_external_goal_mutation_locked().await
        {
            tracing::warn!("failed to prepare external goal mutation: {err}");
        }
        if runtime.is_some() {
            existing = state_db
                .thread_goals()
                .get_thread_goal(thread_id)
                .await
                .map_err(|err| {
                    GoalServiceError::Internal(format!("failed to read thread goal: {err}"))
                })?;
        }
        if let Some(goal) = existing.as_ref()
            && goal_set_request_is_noop(goal, objective, status, token_budget)
        {
            drop(_goal_state_permit);
            return Ok(unchanged_goal_set_outcome(
                goal.clone(),
                runtime,
                external_effect_permit,
            ));
        }
        if objective.is_none() && existing.is_none() {
            return Err(GoalServiceError::InvalidRequest(format!(
                "cannot update goal for thread {thread_id}: no goal exists"
            )));
        }
        let previous_goal_state = existing.clone();
        let previous_goal = previous_goal_state.as_ref().map(PreviousGoalSnapshot::from);
        let runtime_revision = runtime
            .as_ref()
            .map(|runtime| runtime.advance_goal_revision())
            .transpose()
            .map_err(GoalServiceError::Internal)?;

        let goal = if let Some(objective) = objective {
            if let Some(existing_goal) = existing.as_ref() {
                state_db
                    .thread_goals()
                    .update_thread_goal(
                        thread_id,
                        codex_state::GoalUpdate {
                            objective: Some(objective.to_string()),
                            status,
                            token_budget,
                            expected_goal_id: Some(existing_goal.goal_id.clone()),
                        },
                    )
                    .await
                    .map_err(|err| {
                        GoalServiceError::Internal(format!("failed to update thread goal: {err}"))
                    })?
                    .ok_or_else(|| {
                        GoalServiceError::InvalidRequest(format!(
                            "cannot update goal for thread {thread_id}: no goal exists"
                        ))
                    })?
            } else {
                state_db
                    .thread_goals()
                    .replace_thread_goal(
                        thread_id,
                        objective,
                        status.unwrap_or(codex_state::ThreadGoalStatus::Active),
                        token_budget.flatten().or(max_goal_token_budget),
                    )
                    .await
                    .map_err(|err| {
                        GoalServiceError::Internal(format!("failed to replace thread goal: {err}"))
                    })?
            }
        } else {
            let existing_goal = existing.as_ref().ok_or_else(|| {
                GoalServiceError::InvalidRequest(format!(
                    "cannot update goal for thread {thread_id}: no goal exists"
                ))
            })?;
            let expected_goal_id = existing_goal.goal_id.clone();
            state_db
                .thread_goals()
                .update_thread_goal(
                    thread_id,
                    codex_state::GoalUpdate {
                        objective: None,
                        status,
                        token_budget,
                        expected_goal_id: Some(expected_goal_id),
                    },
                )
                .await
                .map_err(|err| {
                    GoalServiceError::Internal(format!("failed to update thread goal: {err}"))
                })?
                .ok_or_else(|| {
                    GoalServiceError::InvalidRequest(format!(
                        "cannot update goal for thread {thread_id}: no goal exists"
                    ))
                })?
        };

        if let Some(runtime) = runtime.as_ref() {
            runtime.clear_pending_turn_start_options().await;
        }

        let runtime_effects = external_goal_set_runtime_effects(
            previous_goal_state.as_ref(),
            &goal,
            runtime
                .as_ref()
                .is_some_and(|runtime| runtime.accounting_state().current_turn_id().is_some()),
        );
        if let (Some(runtime), Some(runtime_revision)) = (runtime.as_ref(), runtime_revision) {
            match runtime_effects.objective_projection {
                GoalObjectiveProjectionEffect::Activated => {
                    runtime.project_active_goal_objective_at_revision(
                        runtime_revision,
                        goal.objective.clone(),
                    );
                }
                GoalObjectiveProjectionEffect::Invalidated => {
                    runtime.clear_active_goal_objective_at_revision(
                        runtime_revision,
                        InactiveGoalHistory::Invalidate,
                    );
                }
                GoalObjectiveProjectionEffect::Preserve
                | GoalObjectiveProjectionEffect::DeferredUntilNextTurn => {}
            }
        }
        if objective.is_some() {
            fill_empty_thread_preview_if_possible(state_db, thread_id, &goal).await;
        }
        drop(_goal_state_permit);
        Ok(GoalSetOutcome {
            goal: protocol_goal_from_state(goal.clone()),
            state_goal: goal,
            previous_goal,
            runtime,
            runtime_revision,
            runtime_effects,
            changed: true,
            external_effect_permit: Arc::new(Mutex::new(Some(external_effect_permit))),
        })
    }

    pub async fn clear_thread_goal(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
    ) -> Result<bool, GoalServiceError> {
        let outcome = self.prepare_thread_goal_clear(state_db, thread_id).await?;
        let cleared = outcome.cleared();
        outcome.apply_runtime_effects().await;
        Ok(cleared)
    }

    pub async fn prepare_thread_goal_clear(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
    ) -> Result<GoalClearOutcome, GoalServiceError> {
        let external_effect_permit = self
            .external_effect_lock(thread_id)
            .acquire_owned()
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?;
        let runtime = self.runtime_for_thread(thread_id);
        // Hold this through the prepare/write window so idle continuation cannot
        // launch from goal state that this external mutation is about to change.
        let goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let mut existing_goal = state_db
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .map_err(|err| {
                GoalServiceError::Internal(format!("failed to read thread goal: {err}"))
            })?;
        if existing_goal.is_none() {
            drop(goal_state_permit);
            return Ok(GoalClearOutcome {
                cleared_goal: None,
                runtime,
                runtime_revision: None,
                external_effect_permit: Mutex::new(Some(external_effect_permit)),
            });
        }
        if let Some(runtime) = runtime.as_ref()
            && let Err(err) = runtime.prepare_external_goal_mutation_locked().await
        {
            tracing::warn!("failed to prepare external goal mutation: {err}");
        }
        if runtime.is_some() {
            existing_goal = state_db
                .thread_goals()
                .get_thread_goal(thread_id)
                .await
                .map_err(|err| {
                    GoalServiceError::Internal(format!("failed to read thread goal: {err}"))
                })?;
        }
        if existing_goal.is_none() {
            drop(goal_state_permit);
            return Ok(GoalClearOutcome {
                cleared_goal: None,
                runtime,
                runtime_revision: None,
                external_effect_permit: Mutex::new(Some(external_effect_permit)),
            });
        }
        let cleared_goal = state_db
            .thread_goals()
            .delete_thread_goal(thread_id)
            .await
            .map_err(|err| {
                GoalServiceError::Internal(format!("failed to clear thread goal: {err}"))
            })?;
        let cleared = cleared_goal.is_some();
        if cleared && let Some(runtime) = runtime.as_ref() {
            runtime.clear_pending_turn_start_options().await;
        }
        let runtime_revision = if cleared_goal.is_some() {
            runtime
                .as_ref()
                .map(|runtime| runtime.advance_goal_revision())
                .transpose()
                .map_err(GoalServiceError::Internal)?
        } else {
            None
        };
        if let (Some(runtime), Some(runtime_revision)) = (runtime.as_ref(), runtime_revision) {
            runtime.clear_active_goal_objective_at_revision(
                runtime_revision,
                InactiveGoalHistory::Invalidate,
            );
        }
        drop(goal_state_permit);
        Ok(GoalClearOutcome {
            cleared_goal,
            runtime,
            runtime_revision,
            external_effect_permit: Mutex::new(Some(external_effect_permit)),
        })
    }
}

fn unchanged_goal_set_outcome(
    goal: codex_state::ThreadGoal,
    runtime: Option<Arc<GoalRuntimeHandle>>,
    external_effect_permit: OwnedSemaphorePermit,
) -> GoalSetOutcome {
    let previous_goal = Some(PreviousGoalSnapshot::from(&goal));
    GoalSetOutcome {
        goal: protocol_goal_from_state(goal.clone()),
        state_goal: goal,
        previous_goal,
        runtime,
        runtime_revision: None,
        runtime_effects: ExternalGoalSetRuntimeEffects {
            objective_projection: GoalObjectiveProjectionEffect::Preserve,
            continuation: GoalContinuationEffect::None,
        },
        changed: false,
        external_effect_permit: Arc::new(Mutex::new(Some(external_effect_permit))),
    }
}

fn goal_set_request_is_noop(
    goal: &codex_state::ThreadGoal,
    objective: Option<&str>,
    status: Option<codex_state::ThreadGoalStatus>,
    token_budget: Option<Option<i64>>,
) -> bool {
    objective.is_none_or(|objective| goal.objective == objective)
        && status.is_none_or(|status| goal.status == status)
        && token_budget.is_none_or(|token_budget| goal.token_budget == token_budget)
}

fn external_goal_set_runtime_effects(
    previous_goal: Option<&codex_state::ThreadGoal>,
    goal: &codex_state::ThreadGoal,
    turn_running: bool,
) -> ExternalGoalSetRuntimeEffects {
    let previous_was_active =
        previous_goal.is_some_and(|goal| goal.status == codex_state::ThreadGoalStatus::Active);
    let goal_is_active = goal.status == codex_state::ThreadGoalStatus::Active;
    let active_projection_changed = goal_is_active
        && (!previous_was_active
            || previous_goal.is_none_or(|previous_goal| previous_goal.goal_id != goal.goal_id)
            || previous_goal
                .is_some_and(|previous_goal| previous_goal.objective != goal.objective));
    let objective_projection = if active_projection_changed {
        if turn_running {
            GoalObjectiveProjectionEffect::DeferredUntilNextTurn
        } else {
            GoalObjectiveProjectionEffect::Activated
        }
    } else if previous_was_active && !goal_is_active {
        GoalObjectiveProjectionEffect::Invalidated
    } else {
        GoalObjectiveProjectionEffect::Preserve
    };
    let continuation = if goal_is_active && !previous_was_active {
        GoalContinuationEffect::StartIfIdle
    } else {
        GoalContinuationEffect::None
    };
    ExternalGoalSetRuntimeEffects {
        objective_projection,
        continuation,
    }
}
