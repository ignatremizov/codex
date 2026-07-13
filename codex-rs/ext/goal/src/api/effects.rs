use super::GoalService;
use crate::runtime::ExternalGoalSetRuntimeEffects;
use crate::runtime::GoalRuntimeHandle;
use crate::runtime::PreviousGoalSnapshot;
use codex_protocol::protocol::ThreadGoal;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use tokio::sync::OwnedSemaphorePermit;

#[derive(Clone, Debug)]
pub struct GoalSetOutcome {
    pub goal: ThreadGoal,
    pub(super) state_goal: codex_state::ThreadGoal,
    pub(super) previous_goal: Option<PreviousGoalSnapshot>,
    pub(super) runtime: Option<Arc<GoalRuntimeHandle>>,
    pub(super) runtime_revision: Option<u64>,
    pub(super) runtime_effects: ExternalGoalSetRuntimeEffects,
    pub(super) changed: bool,
    pub(super) external_effect_permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
}

impl GoalSetOutcome {
    pub async fn acquire_current_effects(&self) -> Option<GoalSetEffects<'_>> {
        let external_effect_permit = self
            .external_effect_permit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        if !self.changed {
            return None;
        }
        let goal_state_permit = match self.runtime.as_ref() {
            Some(runtime) => {
                let permit = match runtime.goal_state_permit().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        tracing::warn!("failed to lock external goal runtime effects: {err}");
                        runtime
                            .fail_closed_external_projection(
                                self.runtime_effects,
                                self.runtime_revision.unwrap_or_default(),
                            )
                            .await;
                        return None;
                    }
                };
                if !self
                    .runtime_revision
                    .is_some_and(|revision| runtime.goal_revision_is(revision))
                {
                    runtime
                        .fail_closed_external_projection(
                            self.runtime_effects,
                            self.runtime_revision.unwrap_or_default(),
                        )
                        .await;
                    return None;
                }
                Some(permit)
            }
            None => None,
        };
        Some(GoalSetEffects {
            outcome: self,
            _external_effect_permit: external_effect_permit,
            _goal_state_permit: goal_state_permit,
        })
    }

    pub async fn apply_runtime_effects(&self, _goal_service: &GoalService) {
        if let Some(effects) = self.acquire_current_effects().await {
            effects.apply_runtime_effects().await;
        }
    }
}

pub struct GoalSetEffects<'a> {
    outcome: &'a GoalSetOutcome,
    _external_effect_permit: OwnedSemaphorePermit,
    _goal_state_permit: Option<OwnedSemaphorePermit>,
}

impl GoalSetEffects<'_> {
    pub async fn apply_runtime_effects(self) {
        let Some(runtime) = self.outcome.runtime.clone() else {
            return;
        };
        let should_continue = match runtime
            .apply_external_goal_set_locked(
                self.outcome.state_goal.clone(),
                self.outcome.previous_goal.clone(),
                self.outcome.runtime_effects,
                self.outcome.runtime_revision.unwrap_or_default(),
            )
            .await
        {
            Ok(should_continue) => should_continue,
            Err(err) => {
                tracing::warn!("failed to apply external goal status runtime effects: {err}");
                false
            }
        };
        drop(self);
        if should_continue && let Err(err) = runtime.continue_if_idle().await {
            tracing::warn!("failed to continue externally activated goal: {err}");
        }
    }
}

#[derive(Debug)]
pub struct GoalClearOutcome {
    pub(super) cleared_goal: Option<codex_state::ThreadGoal>,
    pub(super) runtime: Option<Arc<GoalRuntimeHandle>>,
    pub(super) runtime_revision: Option<u64>,
    pub(super) external_effect_permit: Mutex<Option<OwnedSemaphorePermit>>,
}

impl GoalClearOutcome {
    pub fn cleared(&self) -> bool {
        self.cleared_goal.is_some()
    }

    pub async fn acquire_current_effects(&self) -> Option<GoalClearEffects<'_>> {
        let external_effect_permit = self
            .external_effect_permit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        self.cleared_goal.as_ref()?;
        let goal_state_permit = match self.runtime.as_ref() {
            Some(runtime) => {
                let permit = match runtime.goal_state_permit().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        tracing::warn!("failed to lock external goal-clear runtime effects: {err}");
                        return None;
                    }
                };
                if !self
                    .runtime_revision
                    .is_some_and(|revision| runtime.goal_revision_is(revision))
                {
                    return None;
                }
                Some(permit)
            }
            None => None,
        };
        Some(GoalClearEffects {
            outcome: self,
            _external_effect_permit: external_effect_permit,
            _goal_state_permit: goal_state_permit,
        })
    }

    pub async fn apply_runtime_effects(&self) {
        if let Some(effects) = self.acquire_current_effects().await {
            effects.apply_runtime_effects().await;
        }
    }
}

pub struct GoalClearEffects<'a> {
    outcome: &'a GoalClearOutcome,
    _external_effect_permit: OwnedSemaphorePermit,
    _goal_state_permit: Option<OwnedSemaphorePermit>,
}

impl GoalClearEffects<'_> {
    pub async fn apply_runtime_effects(self) {
        let Some(runtime) = self.outcome.runtime.clone() else {
            return;
        };
        if let Err(err) = runtime
            .apply_external_goal_clear_locked(
                self.outcome.cleared_goal.clone(),
                self.outcome.runtime_revision.unwrap_or_default(),
            )
            .await
        {
            tracing::warn!("failed to apply external goal clear runtime effects: {err}");
        }
    }
}
