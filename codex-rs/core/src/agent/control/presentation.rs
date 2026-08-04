use super::AgentControl;
use crate::agent::response_observation::FinalResponseObservation;
use crate::session::AcceptedCompletionDelivery;
use crate::session::SubmissionAdmission;
use codex_extension_api::ThreadIdleCause;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

mod response_observation;

pub(crate) use self::response_observation::ResponseObservationBinding;
pub(crate) use self::response_observation::ResponseObservationBindingPublication;
pub(crate) use self::response_observation::ResponseObservationDeliveryCommit;
pub(crate) use self::response_observation::ResponseObservationDeliveryKind;
pub(crate) use self::response_observation::ResponseObservationEventMatch;
pub(crate) use self::response_observation::ResponseObservationPersistence;
pub(in crate::agent::control) use self::response_observation::ResponseObserverRelationship;

#[derive(Default)]
pub(super) struct WaitAgentPresentations {
    state: Mutex<PresentationState>,
    response_observation_changed: Notify,
    pub(super) watcher_terminal_changed: Notify,
}

#[derive(Default)]
struct PresentationState {
    next_wait_id: u64,
    next_terminal_presentation_sequence: u64,
    active_targeted_waits: HashMap<(SessionPresentationId, ThreadId), HashSet<u64>>,
    active_any_child_waits: HashMap<SessionPresentationId, HashSet<u64>>,
    wait_parents: HashMap<u64, SessionPresentationId>,
    revoked_wait_parents: HashSet<SessionPresentationId>,
    pending_by_wait: HashMap<u64, Vec<Weak<TerminalPresentationInner>>>,
    // Final-outcome turn IDs remain deduplicated for the live watcher relationship. Recovery can
    // clear one exact ID when it proves that presentation was lost; teardown clears the set.
    terminal_turns_by_observer_child:
        HashMap<(SessionPresentationId, SessionPresentationId), HashSet<String>>,
    watcher_terminals: HashMap<
        (SessionPresentationId, SessionPresentationId),
        VecDeque<WatcherTerminalPresentation>,
    >,
    in_flight_watcher_terminals: HashMap<
        (SessionPresentationId, SessionPresentationId),
        Vec<Weak<TerminalPresentationInner>>,
    >,
    completion_watcher_sessions: HashSet<(SessionPresentationId, SessionPresentationId)>,
    completion_observers_by_child: HashMap<SessionPresentationId, HashSet<SessionPresentationId>>,
    response_observation_by_observer_child:
        HashMap<(SessionPresentationId, SessionPresentationId), ResponseObserverRelationship>,
    completion_delivery_admission_by_child:
        HashMap<(SessionPresentationId, SessionPresentationId), CompletionDeliveryAdmission>,
    response_observation_transactions: HashMap<SessionPresentationId, Arc<AsyncMutex<()>>>,
    trusted_completion_context_response_item_ids: HashMap<ResponseItemId, SessionPresentationId>,
    pending_completion_contexts: HashMap<ResponseItemId, PendingCompletionContext>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionPresentationId {
    pub(crate) thread_id: ThreadId,
    pub(crate) instance_id: Uuid,
}

impl SessionPresentationId {
    pub(crate) fn new(thread_id: ThreadId, instance_id: Uuid) -> Self {
        Self {
            thread_id,
            instance_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalPresentationDelivery {
    Direct,
    Watcher,
}

enum WaitAgentPresentationScope {
    Targeted(Vec<ThreadId>),
    AnyChild,
}

pub(crate) struct WaitAgentPresentationGuard {
    presentations: Arc<WaitAgentPresentations>,
    wait_id: u64,
    parent: SessionPresentationId,
    scope: Option<WaitAgentPresentationScope>,
}

pub(crate) struct CompletionWatcherRegistration {
    presentations: Arc<WaitAgentPresentations>,
    child: SessionPresentationId,
    parent: SessionPresentationId,
    child_lifecycle_generation: u64,
    preserve_state_for_replacement_on_drop: bool,
    active: bool,
}

struct CompletionDeliveryAdmission {
    parent: SessionPresentationId,
    admission: Weak<SubmissionAdmission>,
}

pub(crate) struct WaitAgentPresentationCommit {
    presentations: Arc<WaitAgentPresentations>,
    wait_id: u64,
    parent: SessionPresentationId,
    terminals: Vec<Arc<TerminalPresentationInner>>,
    agent_states: HashMap<ThreadId, AgentStatus>,
    pending_completion_context_ids: Vec<ResponseItemId>,
    committed: bool,
}

pub(crate) struct ClaimedTargetTurn {
    pub(crate) child: SessionPresentationId,
    pub(crate) turn_id: String,
    pub(crate) response_item_id: ResponseItemId,
}

#[derive(Clone)]
pub(crate) struct AgentTerminalPresentation {
    inner: Arc<TerminalPresentationInner>,
}

struct TerminalPresentationInner {
    parent: SessionPresentationId,
    child: SessionPresentationId,
    turn_id: String,
    publication_sequence: u64,
    completion_context_response_item_id: ResponseItemId,
    status: AgentStatus,
    accepted_completion_delivery: Mutex<Option<AcceptedCompletionDelivery>>,
    state: Mutex<TerminalPresentationState>,
    changed: Notify,
}

struct TerminalPresentationState {
    pending_waits: HashSet<u64>,
    wait_committed: bool,
    automatic_delivery_committed: bool,
}

struct PendingCompletionContext {
    parent: SessionPresentationId,
    terminal: Arc<TerminalPresentationInner>,
}

pub(crate) struct WatcherTerminalPresentation {
    pub(crate) turn_id: String,
    pub(crate) status: AgentStatus,
    pub(crate) presentation: AgentTerminalPresentation,
}

#[derive(Clone, Copy)]
pub(super) enum SpawnedThreadRelease {
    Session(SessionPresentationId),
    AbsentThread(ThreadId),
}

impl AgentControl {
    /// Serializes durable observation state for one observer presentation.
    ///
    /// Callers that also need a destination mailbox permit must acquire the mailbox first. This
    /// ordering is shared by lifecycle registration and response delivery so mutually observing
    /// agents cannot form a lock cycle.
    pub(crate) async fn acquire_response_observation_transaction(
        &self,
        parent: SessionPresentationId,
    ) -> OwnedMutexGuard<()> {
        let transaction = self
            .wait_agent_presentations
            .state()
            .response_observation_transactions
            .entry(parent)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        transaction.lock_owned().await
    }

    pub(super) fn release_spawned_thread(&self, release: SpawnedThreadRelease) {
        let child_thread_id = match release {
            SpawnedThreadRelease::Session(child) => child.thread_id,
            SpawnedThreadRelease::AbsentThread(child_thread_id) => child_thread_id,
        };
        self.state.release_spawned_thread(child_thread_id);
        let mut state = self.wait_agent_presentations.state();
        match release {
            SpawnedThreadRelease::Session(child) => {
                state
                    .terminal_turns_by_observer_child
                    .retain(|(_, terminal_child), _| *terminal_child != child);
            }
            SpawnedThreadRelease::AbsentThread(child_thread_id) => {
                state
                    .terminal_turns_by_observer_child
                    .retain(|(_, child), _| child.thread_id != child_thread_id);
            }
        }
    }

    /// Permanently detach runtime response observers for an explicitly closed child.
    ///
    /// V1 watcher recovery deliberately survives transient runtime shutdowns. Explicit close is
    /// different: it revokes that recovery state so an old subscription cannot attach itself to a
    /// later runtime with the same rollout thread ID.
    ///
    /// Returns observers whose bound final wake was removed so their idle lifecycle can be
    /// re-evaluated.
    pub(crate) fn revoke_response_observations_for_child(
        &self,
        child_thread_id: ThreadId,
    ) -> Vec<SessionPresentationId> {
        self.wait_agent_presentations
            .revoke_response_observations_for_child(child_thread_id)
    }

    /// Returns whether this exact presentation relationship had a bound final wake.
    pub(crate) fn revoke_response_observation_for_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        self.wait_agent_presentations
            .revoke_response_observation_for_presentation(parent, child)
    }

    /// Re-emits idle lifecycle only for the still-current live observer presentation.
    pub(crate) async fn recheck_thread_idle_lifecycle(&self, observer: SessionPresentationId) {
        if !self.response_observer_can_retry(observer).await {
            return;
        }
        let Some(state) = self.manager.upgrade() else {
            return;
        };
        if let Ok(thread) = state.get_thread(observer.thread_id).await
            && thread.session.presentation_id() == observer
        {
            // This callback is level-triggered and may also be probed by normal turn completion.
            // GoalRuntime serializes its state transition, and idle-turn reservation atomically
            // prevents two probes from starting duplicate goal turns.
            thread
                .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
                .await;
        }
    }

    pub(crate) fn agent_lifecycle_generation(&self, thread_id: ThreadId) -> u64 {
        self.manager
            .upgrade()
            .map_or(0, |state| state.agent_lifecycle_generation(thread_id))
    }

    pub(crate) fn agent_lifecycle_generation_is_current(
        &self,
        thread_id: ThreadId,
        generation: u64,
    ) -> bool {
        self.manager
            .upgrade()
            .is_some_and(|state| state.agent_lifecycle_generation_is_current(thread_id, generation))
    }

    pub(crate) async fn acquire_current_agent_lifecycle(
        &self,
        thread_id: ThreadId,
        generation: u64,
    ) -> Option<OwnedMutexGuard<()>> {
        let state = self.manager.upgrade()?;
        let lifecycle_lock = state.agent_lifecycle_lock(thread_id);
        let lifecycle_guard = lifecycle_lock.lock_owned().await;
        state
            .agent_lifecycle_generation_is_current(thread_id, generation)
            .then_some(lifecycle_guard)
    }

    pub(crate) fn has_completion_watcher(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .completion_watcher_sessions
            .contains(&(parent, child))
    }

    pub(crate) fn register_targeted_wait_agent_presentation(
        &self,
        parent: SessionPresentationId,
        child_thread_ids: &[ThreadId],
    ) -> WaitAgentPresentationGuard {
        let mut state = self.wait_agent_presentations.state();
        let wait_id = state.next_wait_id();
        if !state.revoked_wait_parents.contains(&parent) {
            state.wait_parents.insert(wait_id, parent);
            for child_thread_id in child_thread_ids {
                state
                    .active_targeted_waits
                    .entry((parent, *child_thread_id))
                    .or_default()
                    .insert(wait_id);
            }
            let pending_terminals = claimable_watcher_terminal_presentations(
                &state,
                |terminal_parent, terminal_child| {
                    terminal_parent == parent
                        && child_thread_ids.contains(&terminal_child.thread_id)
                },
            );
            for terminal in pending_terminals {
                attach_wait_to_pending_terminal(&mut state, wait_id, terminal);
            }
        }
        drop(state);
        WaitAgentPresentationGuard {
            presentations: Arc::clone(&self.wait_agent_presentations),
            wait_id,
            parent,
            scope: Some(WaitAgentPresentationScope::Targeted(
                child_thread_ids.to_vec(),
            )),
        }
    }

    pub(crate) fn register_any_child_wait_agent_presentation(
        &self,
        parent: SessionPresentationId,
    ) -> WaitAgentPresentationGuard {
        let mut state = self.wait_agent_presentations.state();
        let wait_id = state.next_wait_id();
        if !state.revoked_wait_parents.contains(&parent) {
            state.wait_parents.insert(wait_id, parent);
            state
                .active_any_child_waits
                .entry(parent)
                .or_default()
                .insert(wait_id);
            let pending_terminals =
                claimable_watcher_terminal_presentations(&state, |terminal_parent, _| {
                    terminal_parent == parent
                });
            for terminal in pending_terminals {
                attach_wait_to_pending_terminal(&mut state, wait_id, terminal);
            }
        }
        drop(state);
        WaitAgentPresentationGuard {
            presentations: Arc::clone(&self.wait_agent_presentations),
            wait_id,
            parent,
            scope: Some(WaitAgentPresentationScope::AnyChild),
        }
    }

    pub(crate) fn record_agent_terminal_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        status: AgentStatus,
        delivery: TerminalPresentationDelivery,
        on_recorded: impl FnOnce(),
    ) -> Option<AgentTerminalPresentation> {
        let mut state = self.wait_agent_presentations.state();
        let observer_child = (parent, child);
        if state
            .terminal_turns_by_observer_child
            .get(&observer_child)
            .is_some_and(|turn_ids| turn_ids.contains(turn_id))
        {
            return None;
        }

        let mut pending_waits = state
            .active_targeted_waits
            .get(&(parent, child.thread_id))
            .cloned()
            .unwrap_or_default();
        if let Some(wait_ids) = state.active_any_child_waits.get(&parent) {
            pending_waits.extend(wait_ids);
        }
        let accepted_completion_delivery = state
            .completion_delivery_admission_by_child
            .get(&observer_child)
            .filter(|registration| registration.parent == parent)
            .and_then(|registration| registration.admission.upgrade())
            .and_then(|admission| admission.try_accept_completion_delivery());
        let completion_context_response_item_id = state
            .response_observation_by_observer_child
            .get(&observer_child)
            .and_then(|relationship| relationship.turns.get(turn_id))
            .and_then(|observation| observation.final_delivery_response_item_id.clone())
            .unwrap_or_else(new_sub_agent_completion_context_response_item_id);
        let publication_sequence = state.next_terminal_presentation_sequence();
        let presentation = AgentTerminalPresentation {
            inner: Arc::new(TerminalPresentationInner {
                parent,
                child,
                turn_id: turn_id.to_string(),
                publication_sequence,
                completion_context_response_item_id,
                status: status.clone(),
                accepted_completion_delivery: Mutex::new(accepted_completion_delivery),
                state: Mutex::new(TerminalPresentationState {
                    pending_waits: pending_waits.clone(),
                    wait_committed: false,
                    automatic_delivery_committed: false,
                }),
                changed: Notify::new(),
            }),
        };
        for wait_id in pending_waits {
            state
                .pending_by_wait
                .entry(wait_id)
                .or_default()
                .push(Arc::downgrade(&presentation.inner));
        }
        state
            .terminal_turns_by_observer_child
            .entry(observer_child)
            .or_default()
            .insert(turn_id.to_string());
        on_recorded();
        let presentation = match delivery {
            TerminalPresentationDelivery::Direct => Some(presentation),
            TerminalPresentationDelivery::Watcher => {
                state
                    .watcher_terminals
                    .entry(observer_child)
                    .or_default()
                    .push_back(WatcherTerminalPresentation {
                        turn_id: turn_id.to_string(),
                        status,
                        presentation,
                    });
                None
            }
        };
        drop(state);
        if delivery == TerminalPresentationDelivery::Watcher {
            self.wait_agent_presentations
                .watcher_terminal_changed
                .notify_waiters();
        }
        presentation
    }

    pub(crate) fn take_watcher_terminal_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<WatcherTerminalPresentation> {
        let mut state = self.wait_agent_presentations.state();
        let observer_child = (parent, child);
        let (terminal, is_empty) = {
            let terminals = state.watcher_terminals.get_mut(&observer_child)?;
            (terminals.pop_front(), terminals.is_empty())
        };
        if is_empty {
            state.watcher_terminals.remove(&observer_child);
        }
        if let Some(terminal) = &terminal {
            let in_flight = state
                .in_flight_watcher_terminals
                .entry(observer_child)
                .or_default();
            in_flight.retain(|presentation| presentation.strong_count() > 0);
            in_flight.push(Arc::downgrade(&terminal.presentation.inner));
        }
        terminal
    }

    pub(crate) fn requeue_watcher_terminal_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        terminal: WatcherTerminalPresentation,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let observer_child = (parent, child);
        if let Some(in_flight) = state.in_flight_watcher_terminals.get_mut(&observer_child) {
            in_flight.retain(|presentation| {
                presentation.upgrade().is_some_and(|presentation| {
                    !Arc::ptr_eq(&presentation, &terminal.presentation.inner)
                })
            });
            if in_flight.is_empty() {
                state.in_flight_watcher_terminals.remove(&observer_child);
            }
        }
        state
            .watcher_terminals
            .entry(observer_child)
            .or_default()
            .push_front(terminal);
    }

    pub(crate) fn finish_watcher_terminal_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let observer_child = (parent, child);
        if let Some(turn_ids) = state
            .terminal_turns_by_observer_child
            .get_mut(&observer_child)
        {
            turn_ids.remove(turn_id);
            if turn_ids.is_empty() {
                state
                    .terminal_turns_by_observer_child
                    .remove(&observer_child);
            }
        }
        if let Some(in_flight) = state.in_flight_watcher_terminals.get_mut(&observer_child) {
            in_flight.retain(|presentation| {
                presentation
                    .upgrade()
                    .is_some_and(|presentation| presentation.turn_id != turn_id)
            });
            if in_flight.is_empty() {
                state.in_flight_watcher_terminals.remove(&observer_child);
            }
        }
    }

    pub(crate) fn claim_completion_context_response_item_id(
        &self,
        parent: SessionPresentationId,
        id: &ResponseItemId,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let claimed = state
            .trusted_completion_context_response_item_ids
            .get(id)
            .is_some_and(|destination| *destination == parent);
        if claimed {
            state
                .trusted_completion_context_response_item_ids
                .remove(id);
        }
        claimed
    }

    pub(crate) fn is_completion_context_response_item_id_authorized(
        &self,
        parent: SessionPresentationId,
        id: &ResponseItemId,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .trusted_completion_context_response_item_ids
            .get(id)
            .is_some_and(|destination| *destination == parent)
    }

    pub(crate) fn discard_completion_context_response_item_id(&self, id: &ResponseItemId) {
        let mut state = self.wait_agent_presentations.state();
        state
            .trusted_completion_context_response_item_ids
            .remove(id);
        state.pending_completion_contexts.remove(id);
    }

    pub(crate) fn authorize_pending_completion_context(
        &self,
        parent: SessionPresentationId,
        presentation: &AgentTerminalPresentation,
    ) {
        let id = presentation.completion_context_response_item_id();
        let mut state = self.wait_agent_presentations.state();
        state
            .trusted_completion_context_response_item_ids
            .insert(id.clone(), parent);
        state.pending_completion_contexts.insert(
            id,
            PendingCompletionContext {
                parent,
                terminal: Arc::clone(&presentation.inner),
            },
        );
    }

    pub(crate) fn clear_completion_contexts_for_session(&self, session: SessionPresentationId) {
        let mut state = self.wait_agent_presentations.state();
        state
            .trusted_completion_context_response_item_ids
            .retain(|_, destination| *destination != session);
        state
            .pending_completion_contexts
            .retain(|_, context| context.parent != session);
    }

    pub(crate) fn clear_wait_agent_presentations_for_session(
        &self,
        session: SessionPresentationId,
    ) {
        self.wait_agent_presentations
            .cancel_waits_for_parent(session);
        self.wait_agent_presentations
            .state()
            .response_observation_transactions
            .remove(&session);
    }

    pub(crate) fn release_wait_agent_presentations_for_session(
        &self,
        session: SessionPresentationId,
    ) {
        self.wait_agent_presentations.release_wait_parent(session);
    }

    pub(crate) fn completion_parent_for_child(
        &self,
        child: SessionPresentationId,
        declared_parent_thread_id: ThreadId,
    ) -> Option<SessionPresentationId> {
        self.wait_agent_presentations
            .state()
            .completion_observers_by_child
            .get(&child)
            .and_then(|parents| {
                parents
                    .iter()
                    .copied()
                    .find(|parent| parent.thread_id == declared_parent_thread_id)
            })
    }

    pub(crate) fn completion_observers_for_child(
        &self,
        child: SessionPresentationId,
    ) -> Vec<SessionPresentationId> {
        self.wait_agent_presentations
            .state()
            .completion_observers_by_child
            .get(&child)
            .map(|parents| parents.iter().copied().collect())
            .unwrap_or_default()
    }
}

impl WaitAgentPresentationGuard {
    pub(crate) fn freeze_for_children(
        mut self,
        child_thread_ids: impl IntoIterator<Item = ThreadId>,
    ) -> WaitAgentPresentationCommit {
        let children = child_thread_ids.into_iter().collect::<HashSet<_>>();
        self.freeze_latest(|terminal| children.contains(&terminal.child.thread_id))
    }

    pub(crate) fn freeze_for_terminal_statuses(
        mut self,
        terminal_statuses: &HashMap<ThreadId, (Option<String>, AgentStatus)>,
    ) -> WaitAgentPresentationCommit {
        if terminal_statuses.is_empty() {
            return self.freeze_for_children(std::iter::empty());
        }
        let terminals =
            self.presentations
                .freeze_wait(self.wait_id, self.parent, self.scope.take(), |_| true);
        let mut exact = Vec::new();
        let mut latest_without_turn = HashMap::<ThreadId, Arc<TerminalPresentationInner>>::new();
        for terminal in terminals {
            let Some((turn_id, status)) = terminal_statuses.get(&terminal.child.thread_id) else {
                terminal.release(self.wait_id);
                continue;
            };
            if status != &terminal.status {
                terminal.release(self.wait_id);
                continue;
            }
            if let Some(turn_id) = turn_id {
                if turn_id == &terminal.turn_id {
                    // Recovery can reconstruct the same target turn while an older presentation
                    // is still in flight. Claim every copy so no worker can deliver that exact
                    // final outcome after wait_agent returns it once.
                    exact.push(terminal);
                } else {
                    terminal.release(self.wait_id);
                }
                continue;
            }
            let child_thread_id = terminal.child.thread_id;
            match latest_without_turn.entry(child_thread_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(terminal);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().publication_sequence < terminal.publication_sequence {
                        entry.get().release(self.wait_id);
                        entry.insert(terminal);
                    } else {
                        terminal.release(self.wait_id);
                    }
                }
            }
        }
        exact.extend(latest_without_turn.into_values());
        let agent_states = exact
            .iter()
            .map(|terminal| (terminal.child.thread_id, terminal.status.clone()))
            .collect();
        WaitAgentPresentationCommit {
            presentations: Arc::clone(&self.presentations),
            wait_id: self.wait_id,
            parent: self.parent,
            terminals: exact,
            agent_states,
            pending_completion_context_ids: Vec::new(),
            committed: false,
        }
    }

    pub(crate) fn freeze_for_mailbox_response_item_ids(
        mut self,
        response_item_ids: &[ResponseItemId],
    ) -> WaitAgentPresentationCommit {
        let mut commit = self.freeze(|terminal| {
            response_item_ids.contains(&terminal.completion_context_response_item_id)
        });
        let (agent_states, pending_completion_context_ids) = self
            .presentations
            .pending_completion_context_states(self.parent, response_item_ids);
        commit.agent_states.extend(agent_states);
        commit.pending_completion_context_ids = pending_completion_context_ids;
        commit
    }

    pub(crate) fn freeze_none(mut self) -> WaitAgentPresentationCommit {
        self.freeze(|_| false)
    }

    fn freeze(
        &mut self,
        include: impl Fn(&TerminalPresentationInner) -> bool,
    ) -> WaitAgentPresentationCommit {
        let terminals =
            self.presentations
                .freeze_wait(self.wait_id, self.parent, self.scope.take(), include);
        let agent_states = terminals
            .iter()
            .map(|terminal| (terminal.child.thread_id, terminal.status.clone()))
            .collect();
        WaitAgentPresentationCommit {
            presentations: Arc::clone(&self.presentations),
            wait_id: self.wait_id,
            parent: self.parent,
            terminals,
            agent_states,
            pending_completion_context_ids: Vec::new(),
            committed: false,
        }
    }

    fn freeze_latest(
        &mut self,
        include: impl Fn(&TerminalPresentationInner) -> bool,
    ) -> WaitAgentPresentationCommit {
        let terminals =
            self.presentations
                .freeze_wait(self.wait_id, self.parent, self.scope.take(), |_| true);
        let mut latest_by_child = HashMap::<ThreadId, Arc<TerminalPresentationInner>>::new();
        for terminal in terminals {
            if !include(&terminal) {
                terminal.release(self.wait_id);
                continue;
            }
            let child_thread_id = terminal.child.thread_id;
            match latest_by_child.entry(child_thread_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(terminal);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().publication_sequence < terminal.publication_sequence {
                        entry.get().release(self.wait_id);
                        entry.insert(terminal);
                    } else {
                        terminal.release(self.wait_id);
                    }
                }
            }
        }
        let terminals = latest_by_child.into_values().collect::<Vec<_>>();
        let agent_states = terminals
            .iter()
            .map(|terminal| (terminal.child.thread_id, terminal.status.clone()))
            .collect();
        WaitAgentPresentationCommit {
            presentations: Arc::clone(&self.presentations),
            wait_id: self.wait_id,
            parent: self.parent,
            terminals,
            agent_states,
            pending_completion_context_ids: Vec::new(),
            committed: false,
        }
    }
}

impl Drop for WaitAgentPresentationGuard {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.take() {
            self.presentations
                .cancel_wait(self.wait_id, self.parent, scope);
        }
    }
}

impl Drop for CompletionWatcherRegistration {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let presentations = Arc::clone(&self.presentations);
        let mut state = presentations.state();
        self.remove_from_state(&mut state);
    }
}

impl CompletionWatcherRegistration {
    fn remove_from_state(&mut self, state: &mut PresentationState) {
        if !self.active {
            return;
        }
        self.active = false;
        let observer_child = (self.parent, self.child);
        state.completion_watcher_sessions.remove(&observer_child);
        if let Some(parents) = state.completion_observers_by_child.get_mut(&self.child) {
            parents.remove(&self.parent);
            if parents.is_empty() {
                state.completion_observers_by_child.remove(&self.child);
            }
        }
        if state
            .completion_delivery_admission_by_child
            .get(&observer_child)
            .is_some_and(|registration| registration.parent == self.parent)
        {
            state
                .completion_delivery_admission_by_child
                .remove(&observer_child);
        }
        // Only an explicit in-process watcher replacement inherits durable observation state and
        // queued final-outcome presentations. Permanent teardown must release accepted completion
        // admission.
        if !self.preserve_state_for_replacement_on_drop {
            state
                .response_observation_by_observer_child
                .remove(&observer_child);
            state.watcher_terminals.remove(&observer_child);
            state.in_flight_watcher_terminals.remove(&observer_child);
            state
                .terminal_turns_by_observer_child
                .remove(&observer_child);
        }
    }

    /// Atomically retires a one-shot watcher only when no target-turn observation was admitted
    /// while its previous terminal-state delivery was being committed.
    pub(crate) fn retire_if_observation_idle(&mut self) -> bool {
        if !self.active {
            return true;
        }
        let presentations = Arc::clone(&self.presentations);
        let mut state = presentations.state();
        let observer_child = (self.parent, self.child);
        if state
            .response_observation_by_observer_child
            .get(&observer_child)
            .is_some_and(response_observer_relationship_has_work)
        {
            return false;
        }
        self.remove_from_state(&mut state);
        true
    }

    pub(crate) fn preserve_state_for_replacement_on_drop(&mut self) {
        self.preserve_state_for_replacement_on_drop = true;
    }

    pub(crate) fn child_lifecycle_generation(&self) -> u64 {
        self.child_lifecycle_generation
    }
}

fn response_observer_relationship_has_work(relationship: &ResponseObserverRelationship) -> bool {
    relationship.baseline_final_response != FinalResponseObservation::None
        || relationship.pending_next_turn.is_some()
        || !relationship.pending_admissions.is_empty()
        || !relationship.turns.is_empty()
}

impl WaitAgentPresentationCommit {
    pub(crate) fn agent_states(&self) -> HashMap<ThreadId, AgentStatus> {
        self.agent_states.clone()
    }

    pub(crate) fn completion_presentation_agent_ids(&self) -> Option<Vec<ThreadId>> {
        let mut agent_ids = self.agent_states.keys().copied().collect::<Vec<_>>();
        agent_ids.sort_by_key(ToString::to_string);
        (!agent_ids.is_empty()).then_some(agent_ids)
    }

    pub(crate) fn claimed_target_turns(&self) -> Vec<ClaimedTargetTurn> {
        let mut latest_by_target_turn =
            HashMap::<(ThreadId, String), &Arc<TerminalPresentationInner>>::new();
        for terminal in &self.terminals {
            let key = (terminal.child.thread_id, terminal.turn_id.clone());
            match latest_by_target_turn.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(terminal);
                }
                std::collections::hash_map::Entry::Occupied(mut entry)
                    if entry.get().publication_sequence < terminal.publication_sequence =>
                {
                    entry.insert(terminal);
                }
                std::collections::hash_map::Entry::Occupied(_) => {}
            }
        }
        let mut terminals = latest_by_target_turn.into_values().collect::<Vec<_>>();
        terminals.sort_by_key(|terminal| terminal.publication_sequence);
        terminals
            .into_iter()
            .map(|terminal| ClaimedTargetTurn {
                child: terminal.child,
                turn_id: terminal.turn_id.clone(),
                response_item_id: terminal.completion_context_response_item_id.clone(),
            })
            .collect()
    }

    pub(crate) fn commit(mut self) {
        if self
            .presentations
            .commit_wait(self.wait_id, self.parent, self.terminals.as_slice())
        {
            self.presentations
                .remove_pending_completion_contexts(&self.pending_completion_context_ids);
        } else {
            for terminal in &self.terminals {
                terminal.release(self.wait_id);
            }
        }
        self.committed = true;
    }
}

impl Drop for WaitAgentPresentationCommit {
    fn drop(&mut self) {
        if !self.committed {
            self.presentations
                .take_wait_ownership(self.wait_id, self.parent);
            for terminal in &self.terminals {
                terminal.release(self.wait_id);
            }
        }
    }
}

impl AgentTerminalPresentation {
    pub(crate) fn parent(&self) -> SessionPresentationId {
        self.inner.parent
    }

    pub(crate) fn child(&self) -> SessionPresentationId {
        self.inner.child
    }

    pub(crate) fn completion_context_response_item_id(&self) -> ResponseItemId {
        self.inner.completion_context_response_item_id.clone()
    }

    pub(crate) fn take_accepted_completion_delivery(&self) -> Option<AcceptedCompletionDelivery> {
        self.inner
            .accepted_completion_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(crate) fn has_accepted_completion_delivery(&self) -> bool {
        self.inner
            .accepted_completion_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    pub(crate) fn restore_accepted_completion_delivery(
        &self,
        delivery: AcceptedCompletionDelivery,
    ) {
        let mut accepted_completion_delivery = self
            .inner
            .accepted_completion_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if accepted_completion_delivery.is_none() {
            *accepted_completion_delivery = Some(delivery);
        }
    }

    pub(crate) async fn wait_owns_presentation(&self) -> bool {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.inner.state();
                if state.wait_committed {
                    return true;
                }
                if state.pending_waits.is_empty() {
                    state.automatic_delivery_committed = true;
                    return false;
                }
            }
            changed.as_mut().await;
        }
    }
}

impl TerminalPresentationInner {
    fn state(&self) -> std::sync::MutexGuard<'_, TerminalPresentationState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn commit(&self, wait_id: u64) {
        let mut state = self.state();
        if !state.pending_waits.contains(&wait_id) {
            return;
        }
        state.wait_committed = true;
        state.pending_waits.clear();
        drop(state);
        self.changed.notify_waiters();
    }

    fn release(&self, wait_id: u64) {
        let mut state = self.state();
        if !state.pending_waits.remove(&wait_id) {
            return;
        }
        let resolved = state.pending_waits.is_empty();
        drop(state);
        if resolved {
            self.changed.notify_waiters();
        }
    }
}

fn attach_wait_to_pending_terminal(
    state: &mut PresentationState,
    wait_id: u64,
    terminal: Arc<TerminalPresentationInner>,
) {
    let mut terminal_state = terminal.state();
    if terminal_state.wait_committed
        || terminal_state.automatic_delivery_committed
        || !terminal_state.pending_waits.insert(wait_id)
    {
        return;
    }
    drop(terminal_state);
    state
        .pending_by_wait
        .entry(wait_id)
        .or_default()
        .push(Arc::downgrade(&terminal));
}

fn claimable_watcher_terminal_presentations(
    state: &PresentationState,
    include: impl Fn(SessionPresentationId, SessionPresentationId) -> bool,
) -> Vec<Arc<TerminalPresentationInner>> {
    let queued = state
        .watcher_terminals
        .iter()
        .filter(|((parent, child), _)| include(*parent, *child))
        .flat_map(|(_, terminals)| {
            terminals
                .iter()
                .map(|terminal| Arc::clone(&terminal.presentation.inner))
        });
    let in_flight = state
        .in_flight_watcher_terminals
        .iter()
        .filter(|((parent, child), _)| include(*parent, *child))
        .flat_map(|(_, terminals)| terminals.iter().filter_map(Weak::upgrade));
    queued.chain(in_flight).collect()
}

impl WaitAgentPresentations {
    fn state(&self) -> std::sync::MutexGuard<'_, PresentationState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn revoke_response_observations_for_child(
        &self,
        child_thread_id: ThreadId,
    ) -> Vec<SessionPresentationId> {
        let mut state = self.state();
        let affected_wake_observers = state
            .response_observation_by_observer_child
            .iter()
            .filter_map(|((parent, child), relationship)| {
                (child.thread_id == child_thread_id
                    && relationship.turns.values().any(|observation| {
                        observation.final_response == FinalResponseObservation::Wake
                    }))
                .then_some(*parent)
            })
            .collect::<HashSet<_>>();
        state
            .completion_watcher_sessions
            .retain(|(_, child)| child.thread_id != child_thread_id);
        state
            .completion_observers_by_child
            .retain(|child, _| child.thread_id != child_thread_id);
        state
            .completion_delivery_admission_by_child
            .retain(|(_, child), _| child.thread_id != child_thread_id);
        state
            .response_observation_by_observer_child
            .retain(|(_, child), _| child.thread_id != child_thread_id);
        state
            .watcher_terminals
            .retain(|(_, child), _| child.thread_id != child_thread_id);
        state
            .in_flight_watcher_terminals
            .retain(|(_, child), _| child.thread_id != child_thread_id);
        state
            .terminal_turns_by_observer_child
            .retain(|(_, child), _| child.thread_id != child_thread_id);
        let pending_context_ids = state
            .pending_completion_contexts
            .iter()
            .filter_map(|(response_item_id, pending)| {
                (pending.terminal.child.thread_id == child_thread_id)
                    .then_some(response_item_id.clone())
            })
            .collect::<Vec<_>>();
        for response_item_id in pending_context_ids {
            state.pending_completion_contexts.remove(&response_item_id);
            state
                .trusted_completion_context_response_item_ids
                .remove(&response_item_id);
        }
        drop(state);
        self.response_observation_changed.notify_waiters();
        affected_wake_observers.into_iter().collect()
    }

    fn revoke_response_observation_for_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        let mut state = self.state();
        let observer_child = (parent, child);
        let removed_bound_wake = state
            .response_observation_by_observer_child
            .get(&observer_child)
            .is_some_and(|relationship| {
                relationship
                    .turns
                    .values()
                    .any(|observation| observation.final_response == FinalResponseObservation::Wake)
            });
        state.completion_watcher_sessions.remove(&observer_child);
        if let Some(parents) = state.completion_observers_by_child.get_mut(&child) {
            parents.remove(&parent);
            if parents.is_empty() {
                state.completion_observers_by_child.remove(&child);
            }
        }
        state
            .completion_delivery_admission_by_child
            .remove(&observer_child);
        state
            .response_observation_by_observer_child
            .remove(&observer_child);
        state.watcher_terminals.remove(&observer_child);
        state.in_flight_watcher_terminals.remove(&observer_child);
        state
            .terminal_turns_by_observer_child
            .remove(&observer_child);
        let pending_context_ids = state
            .pending_completion_contexts
            .iter()
            .filter_map(|(response_item_id, pending)| {
                (pending.parent == parent && pending.terminal.child == child)
                    .then_some(response_item_id.clone())
            })
            .collect::<Vec<_>>();
        for response_item_id in pending_context_ids {
            state.pending_completion_contexts.remove(&response_item_id);
            state
                .trusted_completion_context_response_item_ids
                .remove(&response_item_id);
        }
        drop(state);
        self.response_observation_changed.notify_waiters();
        removed_bound_wake
    }

    fn freeze_wait(
        &self,
        wait_id: u64,
        parent: SessionPresentationId,
        scope: Option<WaitAgentPresentationScope>,
        include: impl Fn(&TerminalPresentationInner) -> bool,
    ) -> Vec<Arc<TerminalPresentationInner>> {
        let mut state = self.state();
        if let Some(scope) = scope {
            unregister_wait(&mut state, wait_id, parent, &scope);
        }
        let terminals = state
            .pending_by_wait
            .get(&wait_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|terminal| terminal.upgrade())
            .collect::<Vec<_>>();
        drop(state);
        let mut included = Vec::new();
        for terminal in terminals {
            if include(&terminal) {
                included.push(terminal);
            } else {
                terminal.release(wait_id);
            }
        }
        included
    }

    fn cancel_wait(
        &self,
        wait_id: u64,
        parent: SessionPresentationId,
        scope: WaitAgentPresentationScope,
    ) {
        let mut state = self.state();
        unregister_wait(&mut state, wait_id, parent, &scope);
        if state.wait_parents.get(&wait_id) != Some(&parent) {
            return;
        }
        state.wait_parents.remove(&wait_id);
        let terminals = state.pending_by_wait.remove(&wait_id).unwrap_or_default();
        drop(state);
        for terminal in terminals {
            if let Some(terminal) = terminal.upgrade() {
                terminal.release(wait_id);
            }
        }
    }

    fn take_wait_ownership(&self, wait_id: u64, parent: SessionPresentationId) -> bool {
        let mut state = self.state();
        if state.wait_parents.get(&wait_id) != Some(&parent) {
            return false;
        }
        state.wait_parents.remove(&wait_id);
        state.pending_by_wait.remove(&wait_id);
        true
    }

    fn commit_wait(
        &self,
        wait_id: u64,
        parent: SessionPresentationId,
        terminals: &[Arc<TerminalPresentationInner>],
    ) -> bool {
        let mut state = self.state();
        if state.wait_parents.get(&wait_id) != Some(&parent) {
            return false;
        }
        state.wait_parents.remove(&wait_id);
        state.pending_by_wait.remove(&wait_id);
        for terminal in terminals {
            terminal.commit(wait_id);
        }
        true
    }

    fn cancel_waits_for_parent(&self, parent: SessionPresentationId) {
        let terminals = {
            let mut state = self.state();
            state.revoked_wait_parents.insert(parent);
            state
                .active_targeted_waits
                .retain(|(wait_parent, _), _| *wait_parent != parent);
            state
                .active_any_child_waits
                .retain(|wait_parent, _| *wait_parent != parent);
            let wait_ids = state
                .wait_parents
                .iter()
                .filter_map(|(wait_id, wait_parent)| (*wait_parent == parent).then_some(*wait_id))
                .collect::<Vec<_>>();
            let mut terminals = Vec::new();
            for wait_id in wait_ids {
                state.wait_parents.remove(&wait_id);
                terminals.extend(
                    state
                        .pending_by_wait
                        .remove(&wait_id)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|terminal| terminal.upgrade())
                        .map(|terminal| (wait_id, terminal)),
                );
            }
            terminals
        };
        for (wait_id, terminal) in terminals {
            terminal.release(wait_id);
        }
    }

    fn release_wait_parent(&self, parent: SessionPresentationId) {
        self.cancel_waits_for_parent(parent);
        self.state().revoked_wait_parents.remove(&parent);
    }

    fn pending_completion_context_states(
        &self,
        parent: SessionPresentationId,
        response_item_ids: &[ResponseItemId],
    ) -> (HashMap<ThreadId, AgentStatus>, Vec<ResponseItemId>) {
        let state = self.state();
        let pending = response_item_ids
            .iter()
            .filter_map(|response_item_id| {
                state
                    .pending_completion_contexts
                    .get(response_item_id)
                    .filter(|context| context.parent == parent)
                    .map(|context| (response_item_id.clone(), Arc::clone(&context.terminal)))
            })
            .collect::<Vec<_>>();
        let agent_states = pending
            .iter()
            .map(|(_, terminal)| (terminal.child.thread_id, terminal.status.clone()))
            .collect();
        let response_item_ids = pending
            .into_iter()
            .map(|(response_item_id, _)| response_item_id)
            .collect();
        (agent_states, response_item_ids)
    }

    fn remove_pending_completion_contexts(&self, response_item_ids: &[ResponseItemId]) {
        let mut state = self.state();
        for response_item_id in response_item_ids {
            state.pending_completion_contexts.remove(response_item_id);
        }
    }
}

impl PresentationState {
    fn next_wait_id(&mut self) -> u64 {
        let wait_id = self.next_wait_id;
        self.next_wait_id = self.next_wait_id.wrapping_add(1);
        wait_id
    }

    fn next_terminal_presentation_sequence(&mut self) -> u64 {
        let sequence = self.next_terminal_presentation_sequence;
        self.next_terminal_presentation_sequence =
            self.next_terminal_presentation_sequence.wrapping_add(1);
        sequence
    }
}

fn unregister_wait(
    state: &mut PresentationState,
    wait_id: u64,
    parent: SessionPresentationId,
    scope: &WaitAgentPresentationScope,
) {
    match scope {
        WaitAgentPresentationScope::Targeted(child_thread_ids) => {
            for child_thread_id in child_thread_ids {
                remove_wait_id(
                    &mut state.active_targeted_waits,
                    &(parent, *child_thread_id),
                    wait_id,
                );
            }
        }
        WaitAgentPresentationScope::AnyChild => {
            remove_wait_id(&mut state.active_any_child_waits, &parent, wait_id);
        }
    }
}

fn remove_wait_id<K>(waits: &mut HashMap<K, HashSet<u64>>, key: &K, wait_id: u64)
where
    K: Eq + std::hash::Hash,
{
    let Some(wait_ids) = waits.get_mut(key) else {
        return;
    };
    wait_ids.remove(&wait_id);
    if wait_ids.is_empty() {
        waits.remove(key);
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
