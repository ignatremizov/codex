use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use tokio::sync::Semaphore;

use crate::runtime::GoalRuntimeHandle;
use crate::tool::protocol_goal_from_state;

mod effects;
mod mutations;

pub use effects::GoalClearEffects;
pub use effects::GoalClearOutcome;
pub use effects::GoalSetEffects;
pub use effects::GoalSetOutcome;

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

#[derive(Clone, Copy, Debug)]
pub struct GoalSetRequest<'a> {
    pub thread_id: ThreadId,
    pub objective: GoalObjectiveUpdate<'a>,
    pub status: Option<ThreadGoalStatus>,
    pub token_budget: GoalTokenBudgetUpdate,
    pub max_goal_token_budget: Option<i64>,
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

    pub(crate) fn runtime_for_thread(&self, thread_id: ThreadId) -> Option<Arc<GoalRuntimeHandle>> {
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
