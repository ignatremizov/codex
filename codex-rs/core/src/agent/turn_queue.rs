use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use super::control::AgentControl;
use super::control::AgentControlInput;
use super::control::SessionPresentationId;
use super::response_observation::ResponseObservationPolicy;

#[derive(Clone)]
pub(crate) struct QueuedAgentTurn {
    pub(crate) id: Uuid,
    pub(crate) control: AgentControl,
    pub(crate) source: SessionPresentationId,
    pub(crate) target_thread_id: ThreadId,
    pub(crate) input: AgentControlInput,
    pub(crate) start_options: TurnStartOptions,
    pub(crate) response_observation: ResponseObservationPolicy,
    pub(crate) task_preview: Option<String>,
    pub(crate) authored_selector: Option<String>,
    pub(crate) target_message_wake: Option<QueuedTargetMessageWake>,
}

#[derive(Clone)]
pub(crate) struct QueuedTargetMessageWake {
    pub(crate) observer: SessionPresentationId,
    pub(crate) target: SessionPresentationId,
    pub(crate) target_turn_id: String,
    pub(crate) reservation_id: Uuid,
}

impl QueuedAgentTurn {
    pub(crate) fn rollback_target_message_wake(&self) {
        if let Some(reservation) = self.target_message_wake.as_ref() {
            self.control.rollback_target_message_wake_reservation(
                reservation.observer,
                reservation.target,
                &reservation.target_turn_id,
                reservation.reservation_id,
            );
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QueuedAgentTurnView {
    pub(crate) id: Uuid,
    pub(crate) source_thread_id: ThreadId,
    pub(crate) target_thread_id: ThreadId,
    pub(crate) input: Vec<UserInput>,
    pub(crate) prompt_preview: String,
    pub(crate) response_observation: ResponseObservationPolicy,
    pub(crate) authored_selector: Option<String>,
}

#[derive(Default)]
struct AgentTurnQueueState {
    queues: HashMap<ThreadId, VecDeque<QueuedAgentTurn>>,
    in_flight: HashMap<ThreadId, InFlightQueuedAgentTurn>,
    workers: HashSet<ThreadId>,
}

struct InFlightQueuedAgentTurn {
    turn: QueuedAgentTurn,
    admission_started: bool,
}

/// Process-lifetime, target-owned FIFO for distinct future agent turns.
#[derive(Default)]
pub(crate) struct AgentTurnQueue {
    state: Mutex<AgentTurnQueueState>,
    source_admission_locks: Mutex<HashMap<ThreadId, Weak<tokio::sync::Mutex<()>>>>,
    changed: Notify,
}

impl AgentTurnQueue {
    fn source_admission_lock(&self, source_thread_id: ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .source_admission_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks
            .get(&source_thread_id)
            .and_then(std::sync::Weak::upgrade)
        {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(source_thread_id, Arc::downgrade(&lock));
        lock
    }

    pub(crate) async fn acquire_source_admission(
        &self,
        source_thread_id: ThreadId,
    ) -> OwnedMutexGuard<()> {
        self.source_admission_lock(source_thread_id)
            .lock_owned()
            .await
    }

    pub(crate) async fn acquire_source_admissions(
        &self,
        thread_ids: impl IntoIterator<Item = ThreadId>,
    ) -> Vec<OwnedMutexGuard<()>> {
        let mut thread_ids = thread_ids.into_iter().collect::<Vec<_>>();
        thread_ids.sort_by_key(ToString::to_string);
        thread_ids.dedup();
        let mut guards = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            guards.push(self.acquire_source_admission(thread_id).await);
        }
        guards
    }

    pub(crate) fn enqueue(&self, turn: QueuedAgentTurn) -> bool {
        let target_thread_id = turn.target_thread_id;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .queues
            .entry(target_thread_id)
            .or_default()
            .push_back(turn);
        let start_worker = state.workers.insert(target_thread_id);
        drop(state);
        self.changed.notify_waiters();
        start_worker
    }

    pub(crate) fn has_pending(&self, target_thread_id: ThreadId) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight.contains_key(&target_thread_id)
            || state
                .queues
                .get(&target_thread_id)
                .is_some_and(|queue| !queue.is_empty())
    }

    pub(crate) fn has_pending_involving(&self, thread_id: ThreadId) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight.iter().any(|(target_thread_id, in_flight)| {
            *target_thread_id == thread_id || in_flight.turn.source.thread_id == thread_id
        }) || state.queues.iter().any(|(target_thread_id, queue)| {
            *target_thread_id == thread_id
                || queue.iter().any(|turn| turn.source.thread_id == thread_id)
        })
    }

    pub(crate) fn take_front(&self, target_thread_id: ThreadId) -> Option<QueuedAgentTurn> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_flight.contains_key(&target_thread_id) {
            return None;
        }
        let turn = state.queues.get_mut(&target_thread_id)?.pop_front()?;
        state.in_flight.insert(
            target_thread_id,
            InFlightQueuedAgentTurn {
                turn: turn.clone(),
                admission_started: false,
            },
        );
        Some(turn)
    }

    pub(crate) fn begin_admission(&self, target_thread_id: ThreadId, id: Uuid) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(in_flight) = state
            .in_flight
            .get_mut(&target_thread_id)
            .filter(|in_flight| in_flight.turn.id == id)
        else {
            return false;
        };
        // This is the queue's irreversible cancellation claim, before async observer and mailbox
        // setup. Keeping the entry cancellable beyond here could report successful deletion after
        // target input committed. Recoverable setup races restore the entry as pending instead.
        in_flight.admission_started = true;
        true
    }

    pub(crate) fn finish_front(&self, target_thread_id: ThreadId, id: Uuid) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .in_flight
            .get(&target_thread_id)
            .is_some_and(|in_flight| in_flight.turn.id == id)
        {
            state.in_flight.remove(&target_thread_id);
        }
        drop(state);
        self.changed.notify_waiters();
    }

    pub(crate) fn restore_front(&self, target_thread_id: ThreadId, id: Uuid) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .in_flight
            .get(&target_thread_id)
            .is_none_or(|in_flight| in_flight.turn.id != id)
        {
            return;
        }
        let Some(in_flight) = state.in_flight.remove(&target_thread_id) else {
            return;
        };
        state
            .queues
            .entry(target_thread_id)
            .or_default()
            .push_front(in_flight.turn);
        drop(state);
        self.changed.notify_waiters();
    }

    pub(crate) fn stop_worker_if_empty(&self, target_thread_id: ThreadId) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty = !state.in_flight.contains_key(&target_thread_id)
            && state
                .queues
                .get(&target_thread_id)
                .is_none_or(VecDeque::is_empty);
        if empty {
            state.queues.remove(&target_thread_id);
            state.workers.remove(&target_thread_id);
        }
        empty
    }

    pub(crate) fn cancel_for_threads(&self, thread_ids: impl IntoIterator<Item = ThreadId>) {
        let thread_ids = thread_ids.into_iter().collect::<HashSet<_>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cancelled = Vec::new();
        for thread_id in &thread_ids {
            if let Some(queue) = state.queues.remove(thread_id) {
                cancelled.extend(queue);
            }
        }
        state.queues.retain(|_, queue| {
            let mut retained = VecDeque::with_capacity(queue.len());
            while let Some(turn) = queue.pop_front() {
                if thread_ids.contains(&turn.source.thread_id) {
                    cancelled.push(turn);
                } else {
                    retained.push_back(turn);
                }
            }
            *queue = retained;
            !queue.is_empty()
        });
        let cancelled_in_flight = state
            .in_flight
            .iter()
            .filter_map(|(target_thread_id, in_flight)| {
                (!in_flight.admission_started
                    && (thread_ids.contains(target_thread_id)
                        || thread_ids.contains(&in_flight.turn.source.thread_id)))
                .then_some(*target_thread_id)
            })
            .collect::<Vec<_>>();
        for target_thread_id in cancelled_in_flight {
            if let Some(in_flight) = state.in_flight.remove(&target_thread_id) {
                cancelled.push(in_flight.turn);
            }
        }
        drop(state);
        for turn in cancelled {
            turn.rollback_target_message_wake();
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn list_for_root(&self, session_id: SessionId) -> Vec<QueuedAgentTurnView> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut turns = state
            .in_flight
            .values()
            .filter(|in_flight| !in_flight.admission_started)
            .map(|in_flight| &in_flight.turn)
            .chain(state.queues.values().flatten())
            .filter(|turn| turn.control.matches_session_id(session_id))
            .map(|turn| QueuedAgentTurnView {
                id: turn.id,
                source_thread_id: turn.source.thread_id,
                target_thread_id: turn.target_thread_id,
                input: turn.input.presentation().to_vec(),
                prompt_preview: super::control::render_input_preview(turn.input.presentation()),
                response_observation: turn.response_observation,
                authored_selector: turn.authored_selector.clone(),
            })
            .collect::<Vec<_>>();
        // `Uuid::now_v7` is process-monotonic, so this preserves FIFO order while giving
        // app-server pagination a cursor that remains stable as earlier entries drain.
        turns.sort_by_key(|turn| turn.id);
        turns
    }

    pub(crate) fn cancel(&self, session_id: SessionId, id: Uuid) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = None;
        for queue in state.queues.values_mut() {
            if let Some(index) = queue
                .iter()
                .position(|turn| turn.id == id && turn.control.matches_session_id(session_id))
            {
                removed = queue.remove(index);
                break;
            }
        }
        if removed.is_none() {
            let target_thread_id =
                state
                    .in_flight
                    .iter()
                    .find_map(|(target_thread_id, in_flight)| {
                        (!in_flight.admission_started
                            && in_flight.turn.id == id
                            && in_flight.turn.control.matches_session_id(session_id))
                        .then_some(*target_thread_id)
                    });
            if let Some(target_thread_id) = target_thread_id {
                removed = state
                    .in_flight
                    .remove(&target_thread_id)
                    .map(|in_flight| in_flight.turn);
            }
        }
        drop(state);
        let Some(turn) = removed else {
            return false;
        };
        turn.rollback_target_message_wake();
        self.changed.notify_waiters();
        true
    }

    pub(crate) async fn wait_changed(&self) {
        self.changed.notified().await;
    }
}

#[cfg(test)]
#[path = "turn_queue_tests.rs"]
mod tests;
