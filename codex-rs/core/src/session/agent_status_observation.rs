//! Lossless live-status observation, independent of completion acceptance or delivery.

use super::Session;
use codex_protocol::protocol::AgentStatus;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) enum AgentStatusRetirement {
    ExplicitRemoval,
    ResidencyEviction,
    RestoreRollback,
}

#[derive(Default)]
struct Subscribers {
    senders: HashMap<Uuid, mpsc::UnboundedSender<AgentStatus>>,
    closed: bool,
    visible_status: Option<AgentStatus>,
}

#[derive(Default)]
pub(crate) struct AgentStatusObservations {
    subscribers: Arc<Mutex<Subscribers>>,
    suppressed: AtomicBool,
}

/// Owns only an observation lease; it never owns canonical completion presentation.
pub(crate) struct AgentStatusSubscription {
    id: Uuid,
    initial_status: AgentStatus,
    subscribers: Weak<Mutex<Subscribers>>,
    receiver: mpsc::UnboundedReceiver<AgentStatus>,
}

impl AgentStatusSubscription {
    pub(crate) fn initial_status(&self) -> &AgentStatus {
        &self.initial_status
    }

    pub(crate) async fn recv(&mut self) -> Option<AgentStatus> {
        self.receiver.recv().await
    }
}

impl Drop for AgentStatusSubscription {
    fn drop(&mut self) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            subscribers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .senders
                .remove(&self.id);
        }
    }
}

pub(crate) struct AgentStatusObservationSuppressionGuard<'a> {
    suppressed: &'a AtomicBool,
    restore: bool,
}

impl AgentStatusObservationSuppressionGuard<'_> {
    pub(crate) fn keep_suppressed(&mut self) {
        self.restore = false;
    }
}

impl Drop for AgentStatusObservationSuppressionGuard<'_> {
    fn drop(&mut self) {
        if self.restore {
            self.suppressed.store(false, Ordering::Release);
        }
    }
}

impl AgentStatusObservations {
    fn subscribe(&self, mut initial_status: AgentStatus) -> AgentStatusSubscription {
        let id = Uuid::now_v7();
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if subscribers.closed || self.suppressed.load(Ordering::Acquire) {
            initial_status = subscribers
                .visible_status
                .clone()
                .unwrap_or(AgentStatus::PendingInit);
        } else {
            subscribers.visible_status = Some(initial_status.clone());
        }
        if !subscribers.closed {
            subscribers.senders.insert(id, sender);
        }
        AgentStatusSubscription {
            id,
            initial_status,
            subscribers: Arc::downgrade(&self.subscribers),
            receiver,
        }
    }

    fn publish(&self, status: &AgentStatus) {
        if !self.suppressed.load(Ordering::Acquire) {
            let mut subscribers = self
                .subscribers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !subscribers.closed {
                subscribers.visible_status = Some(status.clone());
                subscribers
                    .senders
                    .retain(|_, sender| sender.send(status.clone()).is_ok());
            }
        }
    }

    fn close(&self) {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        subscribers.closed = true;
        subscribers.senders.clear();
    }

    pub(crate) fn suppress(&self) -> AgentStatusObservationSuppressionGuard<'_> {
        AgentStatusObservationSuppressionGuard {
            suppressed: &self.suppressed,
            restore: !self.suppressed.swap(true, Ordering::AcqRel),
        }
    }
}

impl Session {
    pub(crate) fn subscribe_agent_status_events(&self) -> AgentStatusSubscription {
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.agent_status_observations
            .subscribe(self.agent_status.borrow().clone())
    }

    /// Caller holds the terminal publication lock, after immutable terminal acceptance.
    pub(super) fn replace_agent_status_locked(&self, status: AgentStatus) {
        if *self.agent_status.borrow() != status {
            self.agent_status_observations.publish(&status);
            self.agent_status.send_replace(status);
        }
    }

    pub(crate) fn retire_agent_status_observers(&self, reason: AgentStatusRetirement) {
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match reason {
            AgentStatusRetirement::ExplicitRemoval => {
                if !self
                    .agent_status_observations
                    .suppressed
                    .load(Ordering::Acquire)
                {
                    self.capture_adopted_terminal_locked(&AgentStatus::NotFound);
                    self.replace_agent_status_locked(AgentStatus::NotFound);
                }
            }
            AgentStatusRetirement::ResidencyEviction | AgentStatusRetirement::RestoreRollback => {}
        }
        self.agent_status_observations.close();
    }
}

#[cfg(test)]
#[path = "agent_status_observation_tests.rs"]
mod tests;
