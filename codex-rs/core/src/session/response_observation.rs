use super::session::Session;
use super::turn::agent_message_text;
use crate::agent::agent_status_from_event;
use crate::agent::control::ResponseObservationDeliveryCommit;
use crate::agent::status::is_final;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

mod agent_queue_start;
mod delivery;
mod events;
mod recovery;
mod terminal;
use agent_queue_start::AgentQueueTurnStart;
pub(crate) use agent_queue_start::AgentQueueTurnStartPermit;
use events::agent_response_event;
use events::apply_agent_response_event_state;
use events::begin_agent_response_turn_locked;
use events::publish_agent_response_event_locked;
pub(crate) use terminal::TerminalStatusEvent;
pub(crate) use terminal::TerminalStatusSubscription;

#[cfg(test)]
use self::recovery::agent_response_events_from_rollout;
pub(super) use self::recovery::initial_agent_response_observation_state;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentResponseEvent {
    TurnStarted {
        turn_id: String,
        sequence: u64,
    },
    Commentary {
        turn_id: String,
        item_id: String,
        text: String,
        sequence: u64,
    },
    TurnAborted {
        turn_id: String,
    },
    Terminal {
        turn_id: String,
        status: AgentStatus,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentResponseSnapshot {
    pub(crate) active_turn_id: Option<String>,
    pub(crate) last_terminal: Option<(String, AgentStatus)>,
    pub(crate) next_event_sequence: u64,
    pub(crate) last_commentary_item_id: Option<String>,
    pub(crate) status: AgentStatus,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InputTurnAdmissionResolution {
    pub(crate) target_turn_id: String,
    pub(crate) minimum_event_sequence: u64,
    pub(crate) after_item_id: Option<String>,
}

#[derive(Default)]
pub(super) struct AgentResponseObservationState {
    pub(super) task_contexts: HashMap<ResponseItemId, codex_history::UserAgentTaskContextEvidence>,
    pub(super) active_turn_id: Option<String>,
    pub(super) latest_admitted_turn_id: Option<String>,
    pub(super) live_turn_id: Option<String>,
    pub(super) last_terminal: Option<(String, AgentStatus)>,
    pub(super) terminal_outcomes: HashMap<String, AgentStatus>,
    next_event_sequence: u64,
    last_commentary_item_id: Option<String>,
    next_subscriber_id: u64,
    subscribers: HashMap<u64, AgentResponseSubscriber>,
    input_admissions: HashMap<String, oneshot::Sender<CodexResult<InputTurnAdmissionResolution>>>,
    queued_input_starts: HashMap<String, AgentQueueTurnStart>,
    agent_queue_turns: HashMap<String, AgentQueueTurnStart>,
    queued_input_receipts: HashMap<String, oneshot::Sender<CodexResult<()>>>,
    admitted_input_receipts: HashMap<String, oneshot::Sender<CodexResult<()>>>,
    // Exact positive receipts survive replacement history. They never recreate live grants.
    pub(super) settled_completion_contexts:
        HashMap<ResponseItemId, codex_protocol::models::ResponseItem>,
    communication_deliveries: HashMap<ResponseItemId, PendingCommunicationDelivery>,
    consumed_deliveries: std::collections::HashSet<ResponseItemId>,
}

type AgentResponseTerminalObserver = Arc<dyn Fn(&str, AgentStatus) + Send + Sync>;
type AgentResponseStartObserver = Arc<dyn Fn(&str, u64) + Send + Sync>;

struct AgentResponseSubscriber {
    sender: mpsc::UnboundedSender<AgentResponseEvent>,
    terminal_observer: Option<AgentResponseTerminalObserver>,
    start_observer: Option<AgentResponseStartObserver>,
}

struct PendingCommunicationDelivery {
    sender: oneshot::Sender<CodexResult<()>>,
    commit: ResponseObservationDeliveryCommit,
    accepted: Arc<super::AcceptedCompletionDelivery>,
    communication: Option<codex_protocol::protocol::InterAgentCommunication>,
    presentation: Option<crate::agent::control::CompletionPresentation>,
}

pub(crate) struct AgentResponseSubscription {
    id: u64,
    state: std::sync::Weak<Mutex<AgentResponseObservationState>>,
    receiver: mpsc::UnboundedReceiver<AgentResponseEvent>,
}

pub(crate) struct InputTurnAdmission {
    submission_id: String,
    state: std::sync::Weak<Mutex<AgentResponseObservationState>>,
    receiver: Option<oneshot::Receiver<CodexResult<InputTurnAdmissionResolution>>>,
    submitted: bool,
}

pub(crate) struct CommunicationDeliveryReceipt {
    receiver: Option<oneshot::Receiver<CodexResult<()>>>,
    response_item_id: ResponseItemId,
    state: std::sync::Weak<Mutex<AgentResponseObservationState>>,
}

impl AgentResponseSubscription {
    pub(crate) fn try_recv(&mut self) -> Option<AgentResponseEvent> {
        self.receiver.try_recv().ok()
    }

    pub(crate) async fn recv(&mut self) -> Option<AgentResponseEvent> {
        self.receiver.recv().await
    }
}

impl Drop for AgentResponseSubscription {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .subscribers
                .remove(&self.id);
        }
    }
}

impl InputTurnAdmission {
    pub(crate) fn mark_submitted(&mut self) {
        self.submitted = true;
    }

    pub(crate) async fn recv(mut self) -> Option<CodexResult<InputTurnAdmissionResolution>> {
        self.receiver.take()?.await.ok()
    }
}

impl Drop for InputTurnAdmission {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.input_admissions.remove(&self.submission_id);
            if !self.submitted {
                state.queued_input_starts.remove(&self.submission_id);
                state.queued_input_receipts.remove(&self.submission_id);
            }
        }
    }
}

impl CommunicationDeliveryReceipt {
    pub(crate) async fn recv(mut self) -> CodexResult<()> {
        let receiver = self.receiver.take().ok_or(CodexErr::InternalAgentDied)?;
        receiver.await.map_err(|_| CodexErr::InternalAgentDied)?
    }
}

impl Drop for CommunicationDeliveryReceipt {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .communication_deliveries
                .get(&self.response_item_id)
                .is_some_and(|delivery| delivery.communication.is_none())
            {
                state
                    .communication_deliveries
                    .remove(&self.response_item_id);
            }
        }
    }
}

impl Session {
    pub(crate) fn begin_agent_response_turn(&self, turn_id: &str) -> bool {
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .thread_removal_started
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if begin_agent_response_turn_locked(&mut state, turn_id) {
            self.replace_agent_status_locked(AgentStatus::Running);
        }
        true
    }

    pub(crate) fn subscribe_agent_responses(
        &self,
    ) -> (AgentResponseSnapshot, AgentResponseSubscription) {
        let (snapshot, subscription, ()) = self.subscribe_agent_responses_inner(
            /*start_observer*/ None,
            /*terminal_observer*/ None,
            |_| {},
        );
        (snapshot, subscription)
    }

    pub(crate) fn subscribe_agent_responses_observing_turns<R>(
        &self,
        start_observer: impl Fn(&str, u64) + Send + Sync + 'static,
        terminal_observer: impl Fn(&str, AgentStatus) + Send + Sync + 'static,
        on_snapshot: impl FnOnce(&AgentResponseSnapshot) -> R,
    ) -> (AgentResponseSnapshot, AgentResponseSubscription, R) {
        self.subscribe_agent_responses_inner(
            Some(Arc::new(start_observer)),
            Some(Arc::new(terminal_observer)),
            on_snapshot,
        )
    }

    fn subscribe_agent_responses_inner<R>(
        &self,
        start_observer: Option<AgentResponseStartObserver>,
        terminal_observer: Option<AgentResponseTerminalObserver>,
        on_snapshot: impl FnOnce(&AgentResponseSnapshot) -> R,
    ) -> (AgentResponseSnapshot, AgentResponseSubscription, R) {
        // Register the callback and snapshot status on the same side of the final-outcome
        // publication boundary. Otherwise publication can enumerate callbacks, an observer can
        // subscribe and still read Running, and the new observer will miss the only pre-status
        // final-outcome hook.
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tx, receiver) = mpsc::unbounded_channel();
        let id = state.next_subscriber_id;
        state.next_subscriber_id = state.next_subscriber_id.wrapping_add(1);
        if !self
            .thread_removal_started
            .load(std::sync::atomic::Ordering::Acquire)
        {
            state.subscribers.insert(
                id,
                AgentResponseSubscriber {
                    sender: tx,
                    terminal_observer,
                    start_observer,
                },
            );
        }
        let snapshot = AgentResponseSnapshot {
            active_turn_id: state.active_turn_id.clone(),
            last_terminal: state.last_terminal.clone(),
            next_event_sequence: state.next_event_sequence,
            last_commentary_item_id: state.last_commentary_item_id.clone(),
            status: self.agent_status.borrow().clone(),
        };
        let registration = on_snapshot(&snapshot);
        (
            snapshot,
            AgentResponseSubscription {
                id,
                state: Arc::downgrade(&self.response_observation_state),
                receiver,
            },
            registration,
        )
    }

    pub(super) fn close_agent_response_subscriptions(&self) {
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.subscribers.clear();
        for (_, sender) in state.input_admissions.drain() {
            let _ = sender.send(Err(CodexErr::InternalAgentDied));
        }
    }

    pub(super) fn record_agent_response_terminal_observers(
        &self,
        turn_id: &str,
        status: AgentStatus,
    ) {
        if self.agent_status_observations.is_suppressed() {
            return;
        }
        let terminal_observers = {
            let mut state = self
                .response_observation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.terminal_outcomes.contains_key(turn_id) {
                return;
            }
            state
                .terminal_outcomes
                .insert(turn_id.to_string(), status.clone());
            state
                .subscribers
                .values()
                .filter_map(|subscriber| subscriber.terminal_observer.clone())
                .collect::<Vec<_>>()
        };
        for terminal_observer in terminal_observers {
            terminal_observer(turn_id, status.clone());
        }
    }

    pub(crate) fn register_input_turn_admission(
        &self,
        submission_id: String,
    ) -> InputTurnAdmission {
        let (sender, receiver) = oneshot::channel();
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input_admissions
            .insert(submission_id.clone(), sender);
        InputTurnAdmission {
            submission_id,
            state: Arc::downgrade(&self.response_observation_state),
            receiver: Some(receiver),
            submitted: false,
        }
    }

    pub(super) fn capture_input_turn_admission_resolution(
        &self,
        target_turn_id: String,
    ) -> InputTurnAdmissionResolution {
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self
            .thread_removal_started
            .load(std::sync::atomic::Ordering::Acquire)
            && begin_agent_response_turn_locked(&mut state, &target_turn_id)
        {
            self.replace_agent_status_locked(AgentStatus::Running);
        }
        let after_item_id = if state.active_turn_id.as_deref() == Some(target_turn_id.as_str()) {
            state.last_commentary_item_id.clone()
        } else {
            None
        };
        InputTurnAdmissionResolution {
            target_turn_id,
            minimum_event_sequence: state.next_event_sequence,
            after_item_id,
        }
    }

    pub(super) fn resolve_input_turn_admission(
        &self,
        submission_id: &str,
        resolution: InputTurnAdmissionResolution,
    ) {
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(start) = state.queued_input_starts.remove(submission_id) {
            state
                .agent_queue_turns
                .insert(resolution.target_turn_id.clone(), start);
        }
        if let Some(receipt) = state.queued_input_receipts.remove(submission_id) {
            state
                .admitted_input_receipts
                .insert(resolution.target_turn_id.clone(), receipt);
        }
        if let Some(sender) = state.input_admissions.remove(submission_id) {
            let _ = sender.send(Ok(resolution));
        }
    }

    pub(super) fn reject_input_turn_admission(&self, submission_id: &str, error: CodexErr) {
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.queued_input_starts.remove(submission_id);
        state.queued_input_receipts.remove(submission_id);
        if let Some(sender) = state.input_admissions.remove(submission_id) {
            let _ = sender.send(Err(error));
        }
    }

    pub(crate) fn register_communication_delivery(
        &self,
        commit: ResponseObservationDeliveryCommit,
        accepted: super::AcceptedCompletionDelivery,
    ) -> CodexResult<CommunicationDeliveryReceipt> {
        self.check_history_publication()?;
        if commit.parent != self.presentation_id() {
            return Err(CodexErr::InvalidRequest(
                "response delivery belongs to another session instance".to_string(),
            ));
        }
        let response_item_id = commit.response_item_id.clone();
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .communication_deliveries
            .contains_key(&response_item_id)
            || state.consumed_deliveries.contains(&response_item_id)
        {
            return Err(CodexErr::InvalidRequest(
                "response delivery is already registered".to_string(),
            ));
        }
        state.communication_deliveries.insert(
            response_item_id.clone(),
            PendingCommunicationDelivery {
                sender,
                commit,
                accepted: Arc::new(accepted),
                communication: None,
                presentation: None,
            },
        );
        Ok(CommunicationDeliveryReceipt {
            receiver: Some(receiver),
            response_item_id,
            state: Arc::downgrade(&self.response_observation_state),
        })
    }

    /// Caller holds `terminal_publication_lock`, including synchronous turn-start observers.
    pub(super) fn publish_agent_response_event(&self, event: &EventMsg) {
        if self.agent_status_observations.is_suppressed() {
            return;
        }
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sequence = state.next_event_sequence;
        let Some(mut response_event) = agent_response_event(event, sequence) else {
            return;
        };
        if let AgentResponseEvent::Terminal { turn_id, status } = &mut response_event
            && let Some(outcome) = state.terminal_outcomes.get(turn_id)
        {
            *status = outcome.clone();
        }
        if matches!(response_event, AgentResponseEvent::Commentary { .. })
            && (self.check_history_publication().is_err()
                || self.submission_admission.completion_writer_closed())
        {
            return;
        }
        if let AgentResponseEvent::TurnStarted { turn_id, sequence } = &response_event {
            state.live_turn_id = Some(turn_id.clone());
            for subscriber in state.subscribers.values() {
                if let Some(observer) = &subscriber.start_observer {
                    observer(turn_id, *sequence);
                }
            }
        }
        let finished_turn = match &response_event {
            AgentResponseEvent::Terminal { turn_id, .. }
            | AgentResponseEvent::TurnAborted { turn_id } => Some(turn_id.clone()),
            AgentResponseEvent::TurnStarted { .. } | AgentResponseEvent::Commentary { .. } => None,
        };
        publish_agent_response_event_locked(&mut state, response_event);
        drop(state);
        if let Some(turn_id) = finished_turn {
            self.services
                .agent_control
                .finish_target_message_wake(self.presentation_id(), &turn_id);
        }
    }

    pub(super) fn publish_agent_response_terminal(&self, turn_id: String, status: AgentStatus) {
        if self.agent_status_observations.is_suppressed() {
            return;
        }
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = state
            .terminal_outcomes
            .get(&turn_id)
            .cloned()
            .unwrap_or(status);
        publish_agent_response_event_locked(
            &mut state,
            AgentResponseEvent::Terminal {
                turn_id: turn_id.clone(),
                status,
            },
        );
        drop(state);
        self.services
            .agent_control
            .finish_target_message_wake(self.presentation_id(), &turn_id);
    }

    /// Returns whether `turn_id` still owns the session-wide status slot.
    ///
    /// A previous turn can publish its final outcome after a newer turn was admitted. The
    /// historical final outcome remains observable by turn ID, but must not replace the newer
    /// turn's status.
    /// Callers hold `terminal_publication_lock`, preserving the documented lock order.
    pub(super) fn response_turn_can_publish_agent_status(&self, turn_id: &str) -> bool {
        let state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .latest_admitted_turn_id
            .as_deref()
            .is_none_or(|latest_turn_id| latest_turn_id == turn_id)
    }
}

#[cfg(test)]
#[path = "response_observation_tests.rs"]
mod tests;
