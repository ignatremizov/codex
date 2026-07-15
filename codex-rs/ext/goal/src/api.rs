use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;

use codex_protocol::ThreadId;
use codex_protocol::protocol::MAX_GOAL_SKILL_SELECTIONS;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::validate_thread_goal_objective;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use crate::runtime::ExternalGoalSetRuntimeEffects;
use crate::runtime::GoalContinuationEffect;
use crate::runtime::GoalRuntimeHandle;
use crate::runtime::GoalSkillProjectionEffect;
use crate::runtime::InactiveGoalHistory;
use crate::runtime::PreviousGoalSnapshot;
use crate::tool::fill_empty_thread_preview_if_possible;
use crate::tool::protocol_goal_from_state;
use crate::tool::state_status_from_protocol;
use crate::tool::validate_goal_budget;

const MAX_GOAL_SKILL_NAME_CHARS: usize = 128;
const MAX_GOAL_SKILL_PATH_CHARS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalServiceError {
    InvalidRequest(String),
    Internal(String),
}

impl fmt::Display for GoalServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for GoalServiceError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalObjectiveUpdate<'a> {
    Keep,
    Set(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalTokenBudgetUpdate {
    Keep,
    Set(Option<i64>),
}

/// Update semantics for the structured skills attached to a goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalSkillSelectionsUpdate<'a> {
    /// Preserve the currently persisted selection set.
    Keep,
    /// Replace the persisted selection set.
    Set(&'a [codex_protocol::protocol::GoalSkillSelection]),
}

#[derive(Clone, Copy, Debug)]
pub struct GoalSetRequest<'a> {
    pub thread_id: ThreadId,
    pub objective: GoalObjectiveUpdate<'a>,
    pub status: Option<ThreadGoalStatus>,
    pub skills: GoalSkillSelectionsUpdate<'a>,
    pub token_budget: GoalTokenBudgetUpdate,
}

#[derive(Clone, Debug)]
pub struct GoalSetOutcome {
    pub goal: ThreadGoal,
    state_goal: codex_state::ThreadGoal,
    skill_selections: Vec<codex_protocol::protocol::GoalSkillSelection>,
    previous_goal: Option<PreviousGoalSnapshot>,
    runtime: Option<Arc<GoalRuntimeHandle>>,
    runtime_revision: Option<u64>,
    runtime_effects: ExternalGoalSetRuntimeEffects,
    changed: bool,
    external_effect_permit: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
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
                self.outcome.skill_selections.clone(),
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
    cleared_goal: Option<codex_state::ThreadGoal>,
    runtime: Option<Arc<GoalRuntimeHandle>>,
    runtime_revision: Option<u64>,
    external_effect_permit: Mutex<Option<OwnedSemaphorePermit>>,
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

#[derive(Debug, Default)]
pub struct GoalService {
    runtimes: Mutex<HashMap<String, Weak<GoalRuntimeHandle>>>,
    external_effect_locks: Mutex<HashMap<String, Weak<Semaphore>>>,
}

impl GoalService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restores persisted goal state into the registered runtime for `thread_id`.
    pub async fn restore_thread_runtime_after_resume(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), GoalServiceError> {
        let runtime = self.runtime_for_thread(thread_id).ok_or_else(|| {
            GoalServiceError::Internal(format!(
                "goal runtime is unavailable for thread {thread_id}"
            ))
        })?;
        runtime
            .restore_after_resume()
            .await
            .map_err(GoalServiceError::Internal)
    }

    /// Flushes any in-flight goal accounting before a fork copies the source goal snapshot.
    pub async fn flush_thread_goal_progress_for_fork(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), GoalServiceError> {
        let Some(runtime) = self.runtime_for_thread(thread_id) else {
            return Ok(());
        };
        let _goal_state_permit = runtime
            .goal_state_permit()
            .await
            .map_err(GoalServiceError::Internal)?;
        runtime
            .prepare_external_goal_mutation_locked()
            .await
            .map_err(GoalServiceError::Internal)
    }

    pub async fn get_thread_goal(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
    ) -> Result<Option<ThreadGoal>, GoalServiceError> {
        state_db
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .map(|goal| goal.map(protocol_goal_from_state))
            .map_err(|err| GoalServiceError::Internal(format!("failed to read thread goal: {err}")))
    }

    pub async fn set_thread_goal(
        &self,
        state_db: &codex_state::StateRuntime,
        request: GoalSetRequest<'_>,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let GoalSetRequest {
            thread_id,
            objective,
            status,
            skills,
            token_budget,
        } = request;
        let status = status.map(state_status_from_protocol);
        let objective = match objective {
            GoalObjectiveUpdate::Keep => None,
            GoalObjectiveUpdate::Set(objective) => Some(objective.trim()),
        };
        let token_budget = match token_budget {
            GoalTokenBudgetUpdate::Keep => None,
            GoalTokenBudgetUpdate::Set(token_budget) => Some(token_budget),
        };
        if let GoalSkillSelectionsUpdate::Set(selections) = skills {
            validate_goal_skill_selections(selections)?;
        }
        if let Some(objective) = objective {
            validate_thread_goal_objective(objective).map_err(GoalServiceError::InvalidRequest)?;
        }
        if objective.is_some() || token_budget.is_some() {
            validate_goal_budget(token_budget.flatten())
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
            .get_thread_goal_with_skill_selections(thread_id)
            .await
            .map_err(|err| {
                GoalServiceError::Internal(format!("failed to read thread goal: {err}"))
            })?;
        let skill_selections = match skills {
            GoalSkillSelectionsUpdate::Keep => existing
                .as_ref()
                .map(|(_, selections)| selections.clone())
                .unwrap_or_default(),
            GoalSkillSelectionsUpdate::Set(selections) => selections.to_vec(),
        };
        validate_goal_skill_selections(&skill_selections)?;
        if let Some((goal, existing_skill_selections)) = existing.as_ref()
            && goal_set_request_is_noop(
                goal,
                existing_skill_selections,
                objective,
                status,
                skills,
                token_budget,
            )
        {
            drop(_goal_state_permit);
            return Ok(unchanged_goal_set_outcome(
                goal.clone(),
                existing_skill_selections.clone(),
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
                .get_thread_goal_with_skill_selections(thread_id)
                .await
                .map_err(|err| {
                    GoalServiceError::Internal(format!("failed to read thread goal: {err}"))
                })?;
        }
        let skill_selections = match skills {
            GoalSkillSelectionsUpdate::Keep => existing
                .as_ref()
                .map(|(_, selections)| selections.clone())
                .unwrap_or_default(),
            GoalSkillSelectionsUpdate::Set(selections) => selections.to_vec(),
        };
        validate_goal_skill_selections(&skill_selections)?;
        if let Some((goal, existing_skill_selections)) = existing.as_ref()
            && goal_set_request_is_noop(
                goal,
                existing_skill_selections,
                objective,
                status,
                skills,
                token_budget,
            )
        {
            drop(_goal_state_permit);
            return Ok(unchanged_goal_set_outcome(
                goal.clone(),
                existing_skill_selections.clone(),
                runtime,
                external_effect_permit,
            ));
        }
        if objective.is_none() && existing.is_none() {
            return Err(GoalServiceError::InvalidRequest(format!(
                "cannot update goal for thread {thread_id}: no goal exists"
            )));
        }
        let previous_goal_state = existing.as_ref().map(|(goal, _)| goal.clone());
        let previous_skill_selections = existing
            .as_ref()
            .map(|(_, selections)| selections.clone())
            .unwrap_or_default();
        let previous_goal = previous_goal_state.as_ref().map(PreviousGoalSnapshot::from);
        let skill_selections_update = match skills {
            GoalSkillSelectionsUpdate::Keep => None,
            GoalSkillSelectionsUpdate::Set(_) => Some(skill_selections.as_slice()),
        };
        let runtime_revision = runtime
            .as_ref()
            .map(|runtime| runtime.advance_goal_revision())
            .transpose()
            .map_err(GoalServiceError::Internal)?;

        let goal = if let Some(objective) = objective {
            if let Some((existing_goal, _)) = existing.as_ref() {
                state_db
                    .thread_goals()
                    .update_thread_goal_with_skill_selections(
                        thread_id,
                        codex_state::GoalUpdate {
                            objective: Some(objective.to_string()),
                            status,
                            token_budget,
                            expected_goal_id: Some(existing_goal.goal_id.clone()),
                        },
                        skill_selections_update,
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
                    .replace_thread_goal_with_skill_selections(
                        thread_id,
                        objective,
                        status.unwrap_or(codex_state::ThreadGoalStatus::Active),
                        token_budget.flatten(),
                        &skill_selections,
                    )
                    .await
                    .map_err(|err| {
                        GoalServiceError::Internal(format!("failed to replace thread goal: {err}"))
                    })?
            }
        } else {
            let (existing_goal, _) = existing.as_ref().ok_or_else(|| {
                GoalServiceError::InvalidRequest(format!(
                    "cannot update goal for thread {thread_id}: no goal exists"
                ))
            })?;
            let expected_goal_id = existing_goal.goal_id.clone();
            state_db
                .thread_goals()
                .update_thread_goal_with_skill_selections(
                    thread_id,
                    codex_state::GoalUpdate {
                        objective: None,
                        status,
                        token_budget,
                        expected_goal_id: Some(expected_goal_id),
                    },
                    skill_selections_update,
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

        let runtime_effects = external_goal_set_runtime_effects(
            previous_goal_state.as_ref(),
            &previous_skill_selections,
            &goal,
            &skill_selections,
            runtime
                .as_ref()
                .is_some_and(|runtime| runtime.accounting_state().current_turn_id().is_some()),
        );
        if let (Some(runtime), Some(runtime_revision)) = (runtime.as_ref(), runtime_revision) {
            match runtime_effects.skill_projection {
                GoalSkillProjectionEffect::Activated => {
                    runtime.activate_goal_skill_selections_at_revision(
                        runtime_revision,
                        goal.goal_id.clone(),
                        skill_selections.clone(),
                    );
                }
                GoalSkillProjectionEffect::Invalidated => {
                    runtime.clear_goal_skill_activations_at_revision(
                        runtime_revision,
                        InactiveGoalHistory::Invalidate,
                    );
                }
                GoalSkillProjectionEffect::Preserve
                | GoalSkillProjectionEffect::DeferredUntilNextTurn => {}
            }
        }
        if objective.is_some() {
            fill_empty_thread_preview_if_possible(state_db, thread_id, &goal).await;
        }
        drop(_goal_state_permit);
        Ok(GoalSetOutcome {
            goal: protocol_goal_from_state(goal.clone()),
            state_goal: goal,
            skill_selections,
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
            runtime.clear_goal_skill_activations_at_revision(
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

    pub(crate) fn register_runtime(&self, runtime: &Arc<GoalRuntimeHandle>) {
        self.runtimes()
            .insert(runtime.thread_id().to_string(), Arc::downgrade(runtime));
    }

    pub(crate) fn unregister_runtime(&self, runtime: &Arc<GoalRuntimeHandle>) {
        let key = runtime.thread_id().to_string();
        let runtime = Arc::downgrade(runtime);
        let mut runtimes = self.runtimes();
        if runtimes
            .get(&key)
            .is_some_and(|registered| registered.ptr_eq(&runtime))
        {
            runtimes.remove(&key);
        }
    }

    fn runtime_for_thread(&self, thread_id: ThreadId) -> Option<Arc<GoalRuntimeHandle>> {
        let key = thread_id.to_string();
        let mut runtimes = self.runtimes();
        let runtime = runtimes.get(&key).and_then(Weak::upgrade);
        if runtime.is_none() {
            runtimes.remove(&key);
        }
        runtime
    }

    fn external_effect_lock(&self, thread_id: ThreadId) -> Arc<Semaphore> {
        let key = thread_id.to_string();
        let mut locks = self
            .external_effect_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Semaphore::new(/*permits*/ 1));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    fn runtimes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Weak<GoalRuntimeHandle>>> {
        self.runtimes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn unchanged_goal_set_outcome(
    goal: codex_state::ThreadGoal,
    skill_selections: Vec<codex_protocol::protocol::GoalSkillSelection>,
    runtime: Option<Arc<GoalRuntimeHandle>>,
    external_effect_permit: OwnedSemaphorePermit,
) -> GoalSetOutcome {
    let previous_goal = Some(PreviousGoalSnapshot::from(&goal));
    GoalSetOutcome {
        goal: protocol_goal_from_state(goal.clone()),
        state_goal: goal,
        skill_selections,
        previous_goal,
        runtime,
        runtime_revision: None,
        runtime_effects: ExternalGoalSetRuntimeEffects {
            skill_projection: GoalSkillProjectionEffect::Preserve,
            continuation: GoalContinuationEffect::None,
        },
        changed: false,
        external_effect_permit: Arc::new(Mutex::new(Some(external_effect_permit))),
    }
}

fn goal_set_request_is_noop(
    goal: &codex_state::ThreadGoal,
    existing_skill_selections: &[codex_protocol::protocol::GoalSkillSelection],
    objective: Option<&str>,
    status: Option<codex_state::ThreadGoalStatus>,
    skills: GoalSkillSelectionsUpdate<'_>,
    token_budget: Option<Option<i64>>,
) -> bool {
    objective.is_none_or(|objective| goal.objective == objective)
        && status.is_none_or(|status| goal.status == status)
        && token_budget.is_none_or(|token_budget| goal.token_budget == token_budget)
        && match skills {
            GoalSkillSelectionsUpdate::Keep => true,
            GoalSkillSelectionsUpdate::Set(skill_selections) => {
                existing_skill_selections == skill_selections
            }
        }
}

fn external_goal_set_runtime_effects(
    previous_goal: Option<&codex_state::ThreadGoal>,
    previous_skill_selections: &[codex_protocol::protocol::GoalSkillSelection],
    goal: &codex_state::ThreadGoal,
    skill_selections: &[codex_protocol::protocol::GoalSkillSelection],
    turn_running: bool,
) -> ExternalGoalSetRuntimeEffects {
    let previous_was_active =
        previous_goal.is_some_and(|goal| goal.status == codex_state::ThreadGoalStatus::Active);
    let goal_is_active = goal.status == codex_state::ThreadGoalStatus::Active;
    let active_authority_changed = goal_is_active
        && (!previous_was_active
            || previous_goal.is_none_or(|previous_goal| previous_goal.goal_id != goal.goal_id)
            || previous_skill_selections != skill_selections);
    let skill_projection = if active_authority_changed {
        if turn_running {
            GoalSkillProjectionEffect::DeferredUntilNextTurn
        } else {
            GoalSkillProjectionEffect::Activated
        }
    } else if previous_was_active && !goal_is_active {
        GoalSkillProjectionEffect::Invalidated
    } else {
        GoalSkillProjectionEffect::Preserve
    };
    let continuation = if goal_is_active && !previous_was_active {
        GoalContinuationEffect::StartIfIdle
    } else {
        GoalContinuationEffect::None
    };
    ExternalGoalSetRuntimeEffects {
        skill_projection,
        continuation,
    }
}

fn validate_goal_skill_selections(
    selections: &[codex_protocol::protocol::GoalSkillSelection],
) -> Result<(), GoalServiceError> {
    if selections.len() > MAX_GOAL_SKILL_SELECTIONS {
        return Err(GoalServiceError::InvalidRequest(format!(
            "a goal may select at most {MAX_GOAL_SKILL_SELECTIONS} skills"
        )));
    }
    for selection in selections {
        if selection.name.trim().is_empty()
            || selection.name.chars().count() > MAX_GOAL_SKILL_NAME_CHARS
        {
            return Err(GoalServiceError::InvalidRequest(format!(
                "goal skill names must contain 1 to {MAX_GOAL_SKILL_NAME_CHARS} characters"
            )));
        }
        if selection.path.trim().is_empty()
            || selection.path.chars().count() > MAX_GOAL_SKILL_PATH_CHARS
        {
            return Err(GoalServiceError::InvalidRequest(format!(
                "goal skill paths must contain 1 to {MAX_GOAL_SKILL_PATH_CHARS} characters"
            )));
        }
    }
    Ok(())
}
