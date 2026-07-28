use super::AgentControl;
use crate::session::AcceptedCompletionDelivery;
use crate::session::SubmissionAdmission;
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
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct WaitAgentPresentations {
    state: Mutex<PresentationState>,
}

#[derive(Default)]
struct PresentationState {
    next_wait_id: u64,
    active_targeted_waits: HashMap<(SessionPresentationId, ThreadId), HashSet<u64>>,
    active_any_child_waits: HashMap<SessionPresentationId, HashSet<u64>>,
    wait_parents: HashMap<u64, SessionPresentationId>,
    revoked_wait_parents: HashSet<SessionPresentationId>,
    pending_by_wait: HashMap<u64, Vec<Weak<TerminalPresentationInner>>>,
    last_terminal_by_child: HashMap<SessionPresentationId, String>,
    watcher_terminals: HashMap<SessionPresentationId, VecDeque<WatcherTerminalPresentation>>,
    completion_watcher_sessions: HashSet<SessionPresentationId>,
    completion_parent_by_child: HashMap<SessionPresentationId, SessionPresentationId>,
    completion_delivery_admission_by_child:
        HashMap<SessionPresentationId, CompletionDeliveryAdmission>,
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
}

struct CompletionDeliveryAdmission {
    parent: SessionPresentationId,
    admission: Weak<SubmissionAdmission>,
}

enum CompletionDeliveryAdmissionRegistration {
    #[cfg(test)]
    Untracked,
    Tracked(Weak<SubmissionAdmission>),
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

#[derive(Clone)]
pub(crate) struct AgentTerminalPresentation {
    inner: Arc<TerminalPresentationInner>,
}

struct TerminalPresentationInner {
    parent: SessionPresentationId,
    child: SessionPresentationId,
    completion_context_response_item_id: ResponseItemId,
    status: AgentStatus,
    accepted_completion_delivery: Mutex<Option<AcceptedCompletionDelivery>>,
    state: Mutex<TerminalPresentationState>,
    changed: Notify,
}

struct TerminalPresentationState {
    pending_waits: HashSet<u64>,
    wait_committed: bool,
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
    pub(super) fn release_spawned_thread(&self, release: SpawnedThreadRelease) {
        let child_thread_id = match release {
            SpawnedThreadRelease::Session(child) => child.thread_id,
            SpawnedThreadRelease::AbsentThread(child_thread_id) => child_thread_id,
        };
        self.state.release_spawned_thread(child_thread_id);
        let mut state = self.wait_agent_presentations.state();
        match release {
            SpawnedThreadRelease::Session(child) => {
                state.last_terminal_by_child.remove(&child);
            }
            SpawnedThreadRelease::AbsentThread(child_thread_id) => {
                state
                    .last_terminal_by_child
                    .retain(|child, _| child.thread_id != child_thread_id);
            }
        }
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
        if state
            .last_terminal_by_child
            .get(&child)
            .is_some_and(|last_turn_id| last_turn_id == turn_id)
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
            .get(&child)
            .filter(|registration| registration.parent == parent)
            .and_then(|registration| registration.admission.upgrade())
            .and_then(|admission| admission.try_accept_completion_delivery());
        let presentation = AgentTerminalPresentation {
            inner: Arc::new(TerminalPresentationInner {
                parent,
                child,
                completion_context_response_item_id:
                    new_sub_agent_completion_context_response_item_id(),
                status: status.clone(),
                accepted_completion_delivery: Mutex::new(accepted_completion_delivery),
                state: Mutex::new(TerminalPresentationState {
                    pending_waits: pending_waits.clone(),
                    wait_committed: false,
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
            .last_terminal_by_child
            .insert(child, turn_id.to_string());
        on_recorded();
        match delivery {
            TerminalPresentationDelivery::Direct => Some(presentation),
            TerminalPresentationDelivery::Watcher => {
                state.watcher_terminals.entry(child).or_default().push_back(
                    WatcherTerminalPresentation {
                        turn_id: turn_id.to_string(),
                        status,
                        presentation,
                    },
                );
                None
            }
        }
    }

    pub(crate) fn take_watcher_terminal_presentation(
        &self,
        child: SessionPresentationId,
    ) -> Option<WatcherTerminalPresentation> {
        let mut state = self.wait_agent_presentations.state();
        let (terminal, is_empty) = {
            let terminals = state.watcher_terminals.get_mut(&child)?;
            (terminals.pop_front(), terminals.is_empty())
        };
        if is_empty {
            state.watcher_terminals.remove(&child);
        }
        terminal
    }

    pub(crate) fn finish_watcher_terminal_presentation(
        &self,
        child: SessionPresentationId,
        turn_id: &str,
    ) {
        let mut state = self.wait_agent_presentations.state();
        if state
            .last_terminal_by_child
            .get(&child)
            .is_some_and(|last_turn_id| last_turn_id == turn_id)
        {
            state.last_terminal_by_child.remove(&child);
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
    }

    pub(crate) fn release_wait_agent_presentations_for_session(
        &self,
        session: SessionPresentationId,
    ) {
        self.wait_agent_presentations.release_wait_parent(session);
    }

    #[cfg(test)]
    pub(crate) fn register_completion_watcher(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
    ) -> Option<CompletionWatcherRegistration> {
        self.register_completion_watcher_inner(
            child,
            parent,
            CompletionDeliveryAdmissionRegistration::Untracked,
        )
    }

    pub(crate) fn register_completion_watcher_with_admission(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: &Arc<SubmissionAdmission>,
    ) -> Option<CompletionWatcherRegistration> {
        self.register_completion_watcher_inner(
            child,
            parent,
            CompletionDeliveryAdmissionRegistration::Tracked(Arc::downgrade(admission)),
        )
    }

    fn register_completion_watcher_inner(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: CompletionDeliveryAdmissionRegistration,
    ) -> Option<CompletionWatcherRegistration> {
        let mut state = self.wait_agent_presentations.state();
        if !state.completion_watcher_sessions.insert(child) {
            return None;
        }
        state.completion_parent_by_child.insert(child, parent);
        match admission {
            #[cfg(test)]
            CompletionDeliveryAdmissionRegistration::Untracked => {}
            CompletionDeliveryAdmissionRegistration::Tracked(admission) => {
                state
                    .completion_delivery_admission_by_child
                    .insert(child, CompletionDeliveryAdmission { parent, admission });
            }
        }
        Some(CompletionWatcherRegistration {
            presentations: Arc::clone(&self.wait_agent_presentations),
            child,
            parent,
        })
    }

    pub(crate) fn completion_parent_for_child(
        &self,
        child: SessionPresentationId,
        declared_parent_thread_id: ThreadId,
    ) -> Option<SessionPresentationId> {
        self.wait_agent_presentations
            .state()
            .completion_parent_by_child
            .get(&child)
            .copied()
            .filter(|parent| parent.thread_id == declared_parent_thread_id)
    }
}

impl WaitAgentPresentationGuard {
    pub(crate) fn freeze_for_children(
        mut self,
        child_thread_ids: impl IntoIterator<Item = ThreadId>,
    ) -> WaitAgentPresentationCommit {
        let children = child_thread_ids.into_iter().collect::<HashSet<_>>();
        self.freeze(|terminal| children.contains(&terminal.child.thread_id))
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
        let mut state = self.presentations.state();
        state.completion_watcher_sessions.remove(&self.child);
        if state.completion_parent_by_child.get(&self.child) == Some(&self.parent) {
            state.completion_parent_by_child.remove(&self.child);
        }
        if state
            .completion_delivery_admission_by_child
            .get(&self.child)
            .is_some_and(|registration| registration.parent == self.parent)
        {
            state
                .completion_delivery_admission_by_child
                .remove(&self.child);
        }
    }
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

    pub(crate) async fn wait_owns_presentation(&self) -> bool {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let should_wait = {
                let state = self.inner.state();
                if state.wait_committed {
                    return true;
                }
                !state.pending_waits.is_empty()
            };
            if !should_wait {
                return false;
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

impl WaitAgentPresentations {
    fn state(&self) -> std::sync::MutexGuard<'_, PresentationState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
