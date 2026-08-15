use super::session::Session;
use super::turn::agent_message_text;
use crate::agent::agent_status_from_event;
use crate::agent::control::ResponseObservationDeliveryCommit;
use crate::agent::status::is_final;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::rollout::rollout_without_exact_rollback_ranges;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStoreError;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

mod recovery;

pub(crate) use self::recovery::agent_response_events_from_rollout;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum InputTurnAdmissionPolicy {
    #[default]
    AnyTurn,
    IdleOnly,
}

#[derive(Default)]
pub(super) struct AgentResponseObservationState {
    pub(super) active_turn_id: Option<String>,
    pub(super) latest_admitted_turn_id: Option<String>,
    pub(super) last_terminal: Option<(String, AgentStatus)>,
    next_event_sequence: u64,
    last_commentary_item_id: Option<String>,
    next_subscriber_id: u64,
    subscribers: HashMap<u64, AgentResponseSubscriber>,
    input_admissions: HashMap<String, oneshot::Sender<CodexResult<InputTurnAdmissionResolution>>>,
    input_admission_policies: HashMap<String, InputTurnAdmissionPolicy>,
    communication_deliveries: HashMap<ResponseItemId, PendingCommunicationDelivery>,
}

type AgentResponseTerminalObserver = Arc<dyn Fn(&str, AgentStatus) + Send + Sync>;

struct AgentResponseSubscriber {
    sender: mpsc::UnboundedSender<AgentResponseEvent>,
    terminal_observer: Option<AgentResponseTerminalObserver>,
}

struct PendingCommunicationDelivery {
    sender: oneshot::Sender<()>,
    commit: ResponseObservationDeliveryCommit,
}

pub(super) struct PersistedResponseObservationDelivery {
    pub(super) response_item: Option<ResponseItem>,
    pub(super) committed: bool,
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
    response_item_id: ResponseItemId,
    state: std::sync::Weak<Mutex<AgentResponseObservationState>>,
    receiver: Option<oneshot::Receiver<()>>,
}

impl AgentResponseSubscription {
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
    /// Preserve the dispatch policy if the caller stops waiting after the submission was queued.
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
                state.input_admission_policies.remove(&self.submission_id);
            }
        }
    }
}

impl CommunicationDeliveryReceipt {
    pub(crate) async fn recv(mut self) -> bool {
        let Some(receiver) = self.receiver.take() else {
            return false;
        };
        receiver.await.is_ok()
    }
}

impl Drop for CommunicationDeliveryReceipt {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .communication_deliveries
                .remove(&self.response_item_id);
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
        self.subscribe_agent_responses_inner(/*terminal_observer*/ None)
    }

    /// Returns the turn whose response lifecycle is still active, including the short
    /// finalization window after its task has detached from [`crate::state::ActiveTurn`].
    pub(crate) fn active_agent_response_turn_id(&self) -> Option<String> {
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_turn_id
            .clone()
    }

    pub(crate) fn subscribe_agent_responses_observing_terminal(
        &self,
        terminal_observer: impl Fn(&str, AgentStatus) + Send + Sync + 'static,
    ) -> (AgentResponseSnapshot, AgentResponseSubscription) {
        self.subscribe_agent_responses_inner(
            /*terminal_observer*/ Some(Arc::new(terminal_observer)),
        )
    }

    fn subscribe_agent_responses_inner(
        &self,
        terminal_observer: Option<AgentResponseTerminalObserver>,
    ) -> (AgentResponseSnapshot, AgentResponseSubscription) {
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
                },
            );
        }
        (
            AgentResponseSnapshot {
                active_turn_id: state.active_turn_id.clone(),
                last_terminal: state.last_terminal.clone(),
                next_event_sequence: state.next_event_sequence,
                last_commentary_item_id: state.last_commentary_item_id.clone(),
                status: self.agent_status.borrow().clone(),
            },
            AgentResponseSubscription {
                id,
                state: Arc::downgrade(&self.response_observation_state),
                receiver,
            },
        )
    }

    pub(super) fn close_agent_response_subscriptions(&self) {
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribers
            .clear();
    }

    pub(super) fn record_agent_response_terminal_observers(
        &self,
        turn_id: &str,
        status: AgentStatus,
    ) {
        let terminal_observers = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribers
            .values()
            .filter_map(|subscriber| subscriber.terminal_observer.clone())
            .collect::<Vec<_>>();
        for terminal_observer in terminal_observers {
            terminal_observer(turn_id, status.clone());
        }
    }

    pub(crate) fn register_input_turn_admission(
        &self,
        submission_id: String,
        policy: InputTurnAdmissionPolicy,
    ) -> InputTurnAdmission {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.input_admissions.insert(submission_id.clone(), sender);
        state
            .input_admission_policies
            .insert(submission_id.clone(), policy);
        InputTurnAdmission {
            submission_id,
            state: Arc::downgrade(&self.response_observation_state),
            receiver: Some(receiver),
            submitted: false,
        }
    }

    pub(super) fn input_turn_admission_policy(
        &self,
        submission_id: &str,
    ) -> InputTurnAdmissionPolicy {
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input_admission_policies
            .get(submission_id)
            .copied()
            .unwrap_or_default()
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
        state.input_admission_policies.remove(submission_id);
        if let Some(sender) = state.input_admissions.remove(submission_id) {
            let _ = sender.send(Ok(resolution));
        }
    }

    pub(super) fn reject_input_turn_admission(&self, submission_id: &str, error: CodexErr) {
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.input_admission_policies.remove(submission_id);
        if let Some(sender) = state.input_admissions.remove(submission_id) {
            let _ = sender.send(Err(error));
        }
    }

    pub(crate) fn register_communication_delivery(
        &self,
        commit: ResponseObservationDeliveryCommit,
    ) -> CommunicationDeliveryReceipt {
        let response_item_id = commit.response_item_id.clone();
        let (sender, receiver) = oneshot::channel();
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .communication_deliveries
            .insert(
                response_item_id.clone(),
                PendingCommunicationDelivery { sender, commit },
            );
        CommunicationDeliveryReceipt {
            response_item_id,
            state: Arc::downgrade(&self.response_observation_state),
            receiver: Some(receiver),
        }
    }

    pub(crate) fn resolve_communication_delivery(&self, response_item_id: &ResponseItemId) {
        if let Some(delivery) = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .communication_deliveries
            .remove(response_item_id)
        {
            let _ = delivery.sender.send(());
        }
    }

    pub(crate) fn registered_communication_delivery_commit(
        &self,
        response_item_id: &ResponseItemId,
    ) -> Option<ResponseObservationDeliveryCommit> {
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .communication_deliveries
            .get(response_item_id)
            .map(|delivery| delivery.commit.clone())
    }

    pub(crate) fn cancel_communication_delivery(&self, response_item_id: &ResponseItemId) {
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .communication_deliveries
            .remove(response_item_id);
    }

    pub(super) async fn persisted_response_observation_delivery(
        &self,
        response_item_id: &ResponseItemId,
    ) -> Result<PersistedResponseObservationDelivery, ThreadStoreError> {
        if self.live_thread().is_none() {
            return Ok(PersistedResponseObservationDelivery {
                response_item: None,
                committed: false,
            });
        }
        let history = self
            .services
            .thread_store
            .load_rollback_history(LoadThreadHistoryParams {
                thread_id: self.thread_id,
                include_archived: false,
            })
            .await?;
        let items = rollout_without_exact_rollback_ranges(&history.items);
        let response_item = items.iter().rev().find_map(|item| match item {
            RolloutItem::ResponseItem(item) if item.id() == Some(response_item_id) => {
                Some(item.item.clone())
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::AgentResponseObservation(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::TokenUsageRecord(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::EventMsg(_) => None,
        });
        let committed = items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::AgentResponseObservation(observation)
                    if observation
                        .committed_delivery_response_item_ids
                        .contains(response_item_id)
            )
        });
        Ok(PersistedResponseObservationDelivery {
            response_item,
            committed,
        })
    }

    pub(crate) async fn persist_agent_response_observations(
        &self,
        observations: &[AgentResponseObservation],
    ) -> bool {
        if observations.is_empty() {
            return true;
        }
        let Ok(_permit) = self.durable_context_lock.acquire().await else {
            return false;
        };
        self.persist_agent_response_observations_locked(observations)
            .await
    }

    pub(crate) async fn persist_agent_response_observation_replacement(
        &self,
        observations: &[AgentResponseObservation],
    ) -> bool {
        if observations.is_empty() {
            return true;
        }
        let Ok(_permit) = self.durable_context_lock.acquire().await else {
            return false;
        };
        let rollout_items = observations
            .iter()
            .cloned()
            .map(RolloutItem::AgentResponseObservation)
            .collect::<Vec<_>>();
        if let Some(live_thread) = self.live_thread()
            && let Err(err) = live_thread
                .append_items_and_flush_canonical(&rollout_items)
                .await
        {
            tracing::warn!("failed to persist replaced agent response observation state: {err}");
            return false;
        }
        if let Some(task_item) = observations
            .iter()
            .find_map(AgentResponseObservation::promoted_task_context_item)
        {
            let parent_turn = self.new_default_turn().await;
            self.record_into_history(std::slice::from_ref(&task_item), parent_turn.as_ref())
                .await;
        }
        true
    }

    pub(super) async fn persist_agent_response_observations_locked(
        &self,
        observations: &[AgentResponseObservation],
    ) -> bool {
        if observations.is_empty() {
            return true;
        }
        let rollout_items = observations
            .iter()
            .cloned()
            .map(RolloutItem::AgentResponseObservation)
            .collect::<Vec<_>>();
        let result = match self.live_thread() {
            Some(live_thread) => {
                live_thread
                    .append_items_and_flush_canonical(&rollout_items)
                    .await
            }
            None => Ok(()),
        };
        match result {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!("failed to persist agent response observation state: {err}");
                false
            }
        }
    }

    pub(super) fn publish_agent_response_event(&self, event: &EventMsg) {
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sequence = state.next_event_sequence;
        let Some(response_event) = agent_response_event(event, sequence) else {
            return;
        };
        publish_agent_response_event_locked(&mut state, response_event);
    }

    pub(super) fn publish_agent_response_terminal(&self, turn_id: String, status: AgentStatus) {
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_agent_response_event_locked(
            &mut state,
            AgentResponseEvent::Terminal { turn_id, status },
        );
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

fn begin_agent_response_turn_locked(
    state: &mut AgentResponseObservationState,
    turn_id: &str,
) -> bool {
    if state.active_turn_id.as_deref() == Some(turn_id) {
        return false;
    }
    state.active_turn_id = Some(turn_id.to_string());
    state.latest_admitted_turn_id = Some(turn_id.to_string());
    state.last_terminal = None;
    state.last_commentary_item_id = None;
    true
}

fn publish_agent_response_event_locked(
    state: &mut AgentResponseObservationState,
    response_event: AgentResponseEvent,
) {
    state.next_event_sequence = state.next_event_sequence.wrapping_add(1);
    apply_agent_response_event_state(state, &response_event);
    state
        .subscribers
        .retain(|_, subscriber| subscriber.sender.send(response_event.clone()).is_ok());
}

pub(super) fn apply_agent_response_event_state(
    state: &mut AgentResponseObservationState,
    response_event: &AgentResponseEvent,
) {
    match &response_event {
        AgentResponseEvent::TurnStarted { turn_id, .. } => {
            state.active_turn_id = Some(turn_id.clone());
            state.latest_admitted_turn_id = Some(turn_id.clone());
            state.last_terminal = None;
            state.last_commentary_item_id = None;
        }
        AgentResponseEvent::Terminal {
            turn_id, status, ..
        } => {
            if state.latest_admitted_turn_id.is_none() {
                state.latest_admitted_turn_id = Some(turn_id.clone());
            }
            if state.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                state.active_turn_id = None;
                state.last_commentary_item_id = None;
            }
            // A prior turn can finish after a newer turn has started. Preserve that terminal
            // outcome while the newer turn remains active, but never let a delayed historical
            // outcome overwrite the final snapshot of a newer completed turn.
            if state.active_turn_id.is_some()
                || state.latest_admitted_turn_id.as_deref() == Some(turn_id.as_str())
            {
                state.last_terminal = Some((turn_id.clone(), status.clone()));
            }
        }
        AgentResponseEvent::TurnAborted { turn_id, .. } => {
            if state.latest_admitted_turn_id.is_none() {
                state.latest_admitted_turn_id = Some(turn_id.clone());
            }
            if state.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                state.active_turn_id = None;
                state.last_commentary_item_id = None;
            }
        }
        AgentResponseEvent::Commentary {
            turn_id, item_id, ..
        } => {
            if state.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                state.last_commentary_item_id = Some(item_id.clone());
            }
        }
    }
}

fn agent_response_event(event: &EventMsg, sequence: u64) -> Option<AgentResponseEvent> {
    match event {
        EventMsg::TurnStarted(event) => Some(AgentResponseEvent::TurnStarted {
            turn_id: event.turn_id.clone(),
            sequence,
        }),
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(item)
                if matches!(item.phase.as_ref(), Some(MessagePhase::Commentary)) =>
            {
                Some(AgentResponseEvent::Commentary {
                    turn_id: event.turn_id.clone(),
                    item_id: item.id.clone(),
                    text: agent_message_text(item),
                    sequence,
                })
            }
            _ => None,
        },
        EventMsg::TurnComplete(event) => {
            agent_status_from_event(&EventMsg::TurnComplete(event.clone())).map(|status| {
                AgentResponseEvent::Terminal {
                    turn_id: event.turn_id.clone(),
                    status,
                }
            })
        }
        EventMsg::TurnAborted(event) => {
            let turn_id = event.turn_id.clone()?;
            match agent_status_from_event(&EventMsg::TurnAborted(event.clone())) {
                Some(status) if is_final(&status) => {
                    Some(AgentResponseEvent::Terminal { turn_id, status })
                }
                Some(_) => Some(AgentResponseEvent::TurnAborted { turn_id }),
                None => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "response_observation_tests.rs"]
mod tests;
