//! Immutable terminal evidence and exact-session wait ownership.

use super::LocalAgentControl;
use crate::codex_thread::CodexThread;
use crate::session::AcceptedCompletionDelivery;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;
use tokio::sync::Notify;
use uuid::Uuid;

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalPresentationDelivery {
    Direct,
    Watcher,
}

#[derive(Default)]
pub(super) struct WaitAgentPresentations {
    state: Mutex<PresentationState>,
}

#[derive(Default)]
struct PresentationState {
    next_wait: u64,
    waits: HashMap<u64, WaitRegistration>,
    parents: HashMap<SessionPresentationId, ParentBinding>,
    terminal_turns: HashMap<SessionPresentationId, HashSet<String>>,
    queued: HashMap<SessionPresentationId, VecDeque<WatcherTerminalPresentation>>,
    contexts: HashMap<ResponseItemId, Arc<Terminal>>,
}

struct WaitRegistration {
    parent: SessionPresentationId,
    children: Option<HashSet<ThreadId>>,
    terminals: Vec<Weak<Terminal>>,
}

struct ParentBinding {
    thread: Weak<CodexThread>,
    child_reference: String,
}

/// Immutable presentation identity allocated with terminal acceptance, before context writes.
pub(crate) struct CompletionPresentation {
    pub(crate) item: TurnItem,
    pub(crate) history_only_turn_id: String,
}

struct Terminal {
    parent: SessionPresentationId,
    child: SessionPresentationId,
    parent_thread: Mutex<Option<Arc<CodexThread>>>,
    context_id: ResponseItemId,
    status: AgentStatus,
    presentation: CompletionPresentation,
    accepted: Mutex<Option<AcceptedCompletionDelivery>>,
    ownership: Mutex<Ownership>,
    changed: Notify,
}

struct Ownership {
    waits: HashSet<u64>,
    presenter: Option<u64>,
    background_claimed: bool,
    committed: bool,
}

#[derive(Clone)]
pub(crate) struct AgentTerminalPresentation {
    inner: Arc<Terminal>,
}

pub(crate) struct WatcherTerminalPresentation {
    pub(crate) turn_id: String,
    pub(crate) status: AgentStatus,
    pub(crate) presentation: AgentTerminalPresentation,
}

pub(crate) struct CompletionWatcherRegistration {
    state: Arc<WaitAgentPresentations>,
    child: SessionPresentationId,
}

pub(crate) struct WaitAgentPresentationGuard {
    state: Arc<WaitAgentPresentations>,
    id: u64,
    parent: SessionPresentationId,
    armed: bool,
}

pub(crate) struct WaitAgentPresentationCommit {
    state: Arc<WaitAgentPresentations>,
    id: u64,
    parent: SessionPresentationId,
    terminals: Vec<Arc<Terminal>>,
    captured_states: HashMap<ThreadId, AgentStatus>,
}

impl LocalAgentControl {
    pub(crate) fn register_targeted_wait_agent_presentation(
        &self,
        parent: SessionPresentationId,
        children: &[ThreadId],
    ) -> WaitAgentPresentationGuard {
        self.wait_agent_presentations
            .register(parent, Some(children.iter().copied().collect()))
    }

    pub(crate) fn register_any_child_wait_agent_presentation(
        &self,
        parent: SessionPresentationId,
    ) -> WaitAgentPresentationGuard {
        self.wait_agent_presentations.register(parent, None)
    }

    pub(crate) fn register_completion_watcher_with_parent(
        &self,
        child: SessionPresentationId,
        parent: &Arc<CodexThread>,
        child_reference: &str,
    ) -> Option<CompletionWatcherRegistration> {
        let mut state = self
            .wait_agent_presentations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.parents.contains_key(&child) {
            return None;
        }
        state.parents.insert(
            child,
            ParentBinding {
                thread: Arc::downgrade(parent),
                child_reference: child_reference.to_owned(),
            },
        );
        Some(CompletionWatcherRegistration {
            state: Arc::clone(&self.wait_agent_presentations),
            child,
        })
    }

    pub(crate) fn completion_parent_for_child(
        &self,
        child: SessionPresentationId,
        parent_id: ThreadId,
    ) -> Option<SessionPresentationId> {
        self.wait_agent_presentations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .parents
            .get(&child)?
            .thread
            .upgrade()
            .filter(|parent| parent.session.thread_id == parent_id)
            .map(|parent| parent.session.presentation_id())
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
        let mut state = self
            .wait_agent_presentations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state
            .terminal_turns
            .get(&child)
            .is_some_and(|turns| turns.contains(turn_id))
        {
            return None;
        }
        let binding = state.parents.get(&child)?;
        let parent_thread = binding.thread.upgrade()?;
        if parent_thread.session.presentation_id() != parent {
            return None;
        }
        let Some(accepted) = parent_thread
            .session
            .submission_admission
            .try_accept_completion_delivery()
        else {
            on_recorded();
            return None;
        };
        let item =
            codex_protocol::protocol::sub_agent_completion_item(&binding.child_reference, &status)?;
        let presentation = CompletionPresentation {
            item: TurnItem::AgentMessage(item),
            history_only_turn_id: Uuid::now_v7().to_string(),
        };
        let waits = state
            .waits
            .iter()
            .filter_map(|(id, wait)| {
                (wait.parent == parent
                    && wait
                        .children
                        .as_ref()
                        .is_none_or(|children| children.contains(&child.thread_id)))
                .then_some(*id)
            })
            .collect::<HashSet<_>>();
        let inner = Arc::new(Terminal {
            parent,
            child,
            parent_thread: Mutex::new(Some(parent_thread)),
            context_id: new_sub_agent_completion_context_response_item_id(),
            status: status.clone(),
            presentation,
            accepted: Mutex::new(Some(accepted)),
            ownership: Mutex::new(Ownership {
                waits: waits.clone(),
                presenter: None,
                background_claimed: false,
                committed: false,
            }),
            changed: Notify::new(),
        });
        for wait_id in waits {
            if let Some(wait) = state.waits.get_mut(&wait_id) {
                wait.terminals.push(Arc::downgrade(&inner));
            }
        }
        state
            .terminal_turns
            .entry(child)
            .or_default()
            .insert(turn_id.to_string());
        state
            .contexts
            .insert(inner.context_id.clone(), Arc::clone(&inner));
        let presentation = AgentTerminalPresentation { inner };
        // Reservation and immutable evidence are visible before status observers wake.
        on_recorded();
        match delivery {
            TerminalPresentationDelivery::Direct => Some(presentation),
            TerminalPresentationDelivery::Watcher => {
                state
                    .queued
                    .entry(child)
                    .or_default()
                    .push_back(WatcherTerminalPresentation {
                        turn_id: turn_id.to_string(),
                        status,
                        presentation,
                    });
                None
            }
        }
    }

    pub(crate) fn take_watcher_terminal_presentation(
        &self,
        child: SessionPresentationId,
    ) -> Option<WatcherTerminalPresentation> {
        self.wait_agent_presentations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queued
            .get_mut(&child)?
            .pop_front()
    }

    pub(crate) fn clear_wait_agent_presentations_for_session(&self, parent: SessionPresentationId) {
        let waits = {
            let mut state = self
                .wait_agent_presentations
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let ids = state
                .waits
                .iter()
                .filter_map(|(id, wait)| (wait.parent == parent).then_some(*id))
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| state.waits.remove(&id).map(|wait| (id, wait)))
                .collect::<Vec<_>>()
        };
        for (id, wait) in waits {
            for terminal in wait
                .terminals
                .into_iter()
                .filter_map(|terminal| terminal.upgrade())
            {
                terminal.release(id);
            }
        }
    }

    pub(crate) fn clear_completion_contexts_for_session(&self, parent: SessionPresentationId) {
        self.wait_agent_presentations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contexts
            .retain(|_, terminal| terminal.parent != parent);
    }

    pub(crate) fn claim_completion_context_response_item_id(
        &self,
        parent: SessionPresentationId,
        id: &ResponseItemId,
    ) -> bool {
        let mut state = self
            .wait_agent_presentations
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state
            .contexts
            .get(id)
            .is_some_and(|terminal| terminal.parent == parent)
        {
            state.contexts.remove(id);
            return true;
        }
        false
    }
}

impl WaitAgentPresentations {
    fn register(
        self: &Arc<Self>,
        parent: SessionPresentationId,
        children: Option<HashSet<ThreadId>>,
    ) -> WaitAgentPresentationGuard {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let id = state.next_wait;
        state.next_wait += 1;
        state.waits.insert(
            id,
            WaitRegistration {
                parent,
                children,
                terminals: Vec::new(),
            },
        );
        WaitAgentPresentationGuard {
            state: Arc::clone(self),
            id,
            parent,
            armed: true,
        }
    }
}

impl WaitAgentPresentationGuard {
    pub(crate) fn freeze_for_children(
        self,
        children: impl IntoIterator<Item = ThreadId>,
    ) -> WaitAgentPresentationCommit {
        let children = children.into_iter().collect::<HashSet<_>>();
        self.freeze(|terminal| children.contains(&terminal.child.thread_id), &[])
    }

    pub(crate) fn freeze_for_mailbox_response_item_ids(
        self,
        ids: &[ResponseItemId],
    ) -> WaitAgentPresentationCommit {
        self.freeze(|terminal| ids.contains(&terminal.context_id), ids)
    }

    pub(crate) fn freeze_none(self) -> WaitAgentPresentationCommit {
        self.freeze(|_| false, &[])
    }

    fn freeze(
        mut self,
        select: impl Fn(&Terminal) -> bool,
        ids: &[ResponseItemId],
    ) -> WaitAgentPresentationCommit {
        let mut state = self
            .state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let parent = state.waits.get(&self.id).map(|wait| wait.parent);
        let mut terminals = state
            .waits
            .get(&self.id)
            .map(|wait| {
                wait.terminals
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in ids {
            if let Some(terminal) = state.contexts.get(id)
                && Some(terminal.parent) == parent
                && !terminals
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, terminal))
            {
                terminals.push(Arc::clone(terminal));
            }
        }
        let captured_states = terminals
            .iter()
            .filter(|terminal| select(terminal))
            .map(|terminal| (terminal.child.thread_id, terminal.status.clone()))
            .collect();
        terminals.retain(|terminal| {
            let mut ownership = terminal
                .ownership
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let selected = select(terminal)
                && !ownership.committed
                && !ownership.background_claimed
                && ownership
                    .presenter
                    .is_none_or(|presenter| presenter == self.id);
            if !selected {
                ownership.waits.remove(&self.id);
                terminal.changed.notify_waiters();
            } else {
                ownership.waits.insert(self.id);
                ownership.presenter = Some(self.id);
            }
            selected
        });
        // Keep the registration until commit/drop, so manager removal can revoke it.
        if let Some(wait) = state.waits.get_mut(&self.id) {
            wait.terminals = terminals.iter().map(Arc::downgrade).collect();
        }
        let commit = WaitAgentPresentationCommit {
            state: Arc::clone(&self.state),
            id: self.id,
            parent: self.parent,
            terminals,
            captured_states,
        };
        drop(state);
        self.armed = false;
        commit
    }
}

impl Drop for WaitAgentPresentationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(wait) = self
            .state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .waits
            .remove(&self.id)
        {
            for terminal in wait
                .terminals
                .into_iter()
                .filter_map(|terminal| terminal.upgrade())
            {
                terminal.release(self.id);
            }
        }
    }
}

impl WaitAgentPresentationCommit {
    pub(crate) fn agent_states(&self) -> HashMap<ThreadId, AgentStatus> {
        self.captured_states.clone()
    }

    pub(crate) fn completion_presentation_agent_ids(&self) -> Option<Vec<ThreadId>> {
        let mut ids = self
            .terminals
            .iter()
            .map(|terminal| terminal.child.thread_id)
            .collect::<Vec<_>>();
        ids.sort_by_key(ToString::to_string);
        ids.dedup();
        (!ids.is_empty()).then_some(ids)
    }

    pub(crate) fn commit(self) {
        let mut state = self
            .state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state
            .waits
            .get(&self.id)
            .is_some_and(|wait| wait.parent == self.parent)
        {
            state.waits.remove(&self.id);
            for terminal in &self.terminals {
                let mut ownership = terminal
                    .ownership
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                ownership.committed = true;
                ownership.waits.remove(&self.id);
                terminal.changed.notify_waiters();
                state.contexts.remove(&terminal.context_id);
            }
        }
    }
}

impl Drop for WaitAgentPresentationCommit {
    fn drop(&mut self) {
        self.state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .waits
            .remove(&self.id);
        for terminal in &self.terminals {
            terminal.release(self.id);
        }
    }
}

impl Drop for CompletionWatcherRegistration {
    fn drop(&mut self) {
        let mut state = self
            .state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.parents.remove(&self.child);
        state.terminal_turns.remove(&self.child);
        state.queued.remove(&self.child);
    }
}

impl Terminal {
    fn release(&self, id: u64) {
        let mut ownership = self
            .ownership
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ownership.waits.remove(&id);
        if ownership.presenter == Some(id) {
            ownership.presenter = None;
        }
        self.changed.notify_waiters();
    }
}

impl AgentTerminalPresentation {
    pub(crate) fn completion_presentation(&self) -> &CompletionPresentation {
        &self.inner.presentation
    }
    pub(crate) fn parent(&self) -> SessionPresentationId {
        self.inner.parent
    }
    pub(crate) fn take_parent_thread(&self) -> Option<Arc<CodexThread>> {
        self.inner
            .parent_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
    pub(crate) fn completion_context_response_item_id(&self) -> ResponseItemId {
        self.inner.context_id.clone()
    }
    pub(crate) fn take_accepted_completion_delivery(&self) -> Option<AcceptedCompletionDelivery> {
        self.inner
            .accepted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
    pub(crate) async fn wait_owns_presentation(&self) -> bool {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self
                    .inner
                    .ownership
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if state.committed {
                    return true;
                }
                if state.waits.is_empty() {
                    state.background_claimed = true;
                    return false;
                }
            }
            changed.await;
        }
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
