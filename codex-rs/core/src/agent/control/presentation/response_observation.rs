use super::*;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::AgentResponseCommentaryAdmission;
use codex_protocol::protocol::AgentResponseCommentaryDelivery;
use codex_protocol::protocol::AgentResponseObservation;

mod delivery;
mod runtime;
mod snapshot;
mod user_policy;
pub(in crate::agent) use user_policy::ReplacedFinalResponseObservationBinding;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ResponseTurnObservation {
    pub(super) task_preview: Option<String>,
    pub(super) promoted_task_context:
        Option<codex_protocol::protocol::AgentResponsePromotedTaskContext>,
    pub(super) commentary_admissions: Vec<AgentResponseCommentaryAdmission>,
    pub(super) commentary_delivery: Option<AgentResponseCommentaryDelivery>,
    pub(super) commentary_delivery_route: CommentaryDeliveryRoute,
    pub(super) final_response: FinalResponseObservation,
    pub(super) target_messages: bool,
    pub(super) queue_delivery: bool,
    pub(super) message_wake_reservation_id: Option<Uuid>,
    pub(super) message_wake_turn_id: Option<String>,
    pub(super) final_delivery_response_item_id: Option<ResponseItemId>,
    pub(super) committed_delivery_response_item_ids: Vec<ResponseItemId>,
}

impl Default for ResponseTurnObservation {
    fn default() -> Self {
        Self {
            task_preview: None,
            promoted_task_context: None,
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            commentary_delivery_route: CommentaryDeliveryRoute::Undecided,
            final_response: FinalResponseObservation::None,
            target_messages: false,
            queue_delivery: false,
            message_wake_reservation_id: None,
            message_wake_turn_id: None,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommentaryDeliveryRoute {
    #[default]
    Undecided,
    Mailbox,
    Wait,
}

impl ResponseTurnObservation {
    fn new(
        policy: ResponseObservationPolicy,
        minimum_event_sequence: u64,
        after_item_id: Option<String>,
    ) -> Self {
        let mut observation = Self {
            final_response: policy.final_response(),
            ..Default::default()
        };
        observation.merge(policy, minimum_event_sequence, after_item_id);
        observation
    }

    fn merge(
        &mut self,
        policy: ResponseObservationPolicy,
        minimum_event_sequence: u64,
        after_item_id: Option<String>,
    ) {
        if policy.commentary() {
            self.commentary_admissions
                .push(AgentResponseCommentaryAdmission {
                    minimum_event_sequence,
                    after_item_id,
                    canonical_boundary: true,
                });
        }
        self.final_response = self.final_response.max(policy.final_response());
        if policy.target_messages() && !self.target_messages {
            self.message_wake_reservation_id = None;
            self.message_wake_turn_id = None;
        }
        self.target_messages |= policy.target_messages();
        self.queue_delivery |= policy.queue_input();
    }
}

#[derive(Clone)]
pub(in crate::agent::control) struct ResponseObserverRelationship {
    pub(super) revoked: bool,
    pub(super) persistence: ResponseObservationPersistence,
    pub(super) baseline_final_response: FinalResponseObservation,
    pub(super) pending_next_turn: Option<ResponseTurnObservation>,
    pub(super) pending_admissions: HashMap<Uuid, ResponseTurnObservation>,
    pub(super) turns: HashMap<String, ResponseTurnObservation>,
}

impl Default for ResponseObserverRelationship {
    fn default() -> Self {
        Self {
            revoked: false,
            persistence: ResponseObservationPersistence::RuntimeOnly,
            baseline_final_response: FinalResponseObservation::None,
            pending_next_turn: None,
            pending_admissions: HashMap::new(),
            turns: HashMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseObservationBinding {
    NextTurn,
    ExplicitAdmission(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseObservationBindingPublication {
    Immediate,
    Deferred,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ResponseObservationPersistence {
    #[default]
    RuntimeOnly,
    Durable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseObservationEventMatch {
    Observe,
    Ignore,
    AwaitBinding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseObservationDeliveryKind {
    Commentary,
    Final,
}

/// Identifies a durably claimed response whose committed snapshot is written when the observer
/// consumes its mailbox item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResponseObservationDeliveryCommit {
    pub(crate) parent: SessionPresentationId,
    pub(crate) child: SessionPresentationId,
    pub(crate) turn_id: String,
    pub(crate) response_item_id: ResponseItemId,
    pub(crate) kind: ResponseObservationDeliveryKind,
}

pub(crate) struct ResponseWatcherRegistration {
    state: Arc<WaitAgentPresentations>,
    parent: SessionPresentationId,
    child: SessionPresentationId,
    active: bool,
    preserve_state: bool,
}

impl LocalAgentControl {
    /// Returns whether `observer` has a wake-capable final observation bound to concrete work.
    ///
    /// Pending next-turn policies do not count: an idle target may never start that turn, so
    /// treating an unbound policy as pending work could indefinitely defer other automatic work.
    pub(crate) fn has_bound_final_response_wake(&self, observer: SessionPresentationId) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .iter()
            .any(|((parent, _), relationship)| {
                *parent == observer && relationship_has_bound_final_response_wake(relationship)
            })
    }

    /// Installs a policy only for an already validated, exact live observer/target pair.
    /// The caller holds the target lifecycle and observer transaction guards.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_response_watcher_with_parent_at_sequence(
        &self,
        child: SessionPresentationId,
        observer: &Arc<CodexThread>,
        response_observation: ResponseObservationPolicy,
        retain_passive_completion_relationship: bool,
        target_turn_id: Option<String>,
        pending_binding: ResponseObservationBinding,
        persistence: ResponseObservationPersistence,
        minimum_event_sequence: u64,
        after_item_id: Option<String>,
    ) -> Option<ResponseWatcherRegistration> {
        let parent = observer.session.presentation_id();
        if parent.instance_id.is_nil() || child.instance_id.is_nil() {
            return None;
        }
        let mut state = self.wait_agent_presentations.state();
        let observer_child = (parent, child);
        let relationship = state
            .response_observation_by_observer_child
            .entry(observer_child)
            .or_default();
        if relationship.revoked {
            return None;
        }
        relationship.persistence = relationship.persistence.max(persistence);
        if retain_passive_completion_relationship
            && response_observation.final_response() != FinalResponseObservation::None
        {
            // The requested policy, including the omitted-`w` passive default, is installed
            // directly on the pending or bound turn below. This baseline is only the passive
            // fallback for later turns in a retained watcher relationship; it does not downgrade
            // a wake policy on the current turn.
            relationship.baseline_final_response = FinalResponseObservation::Passive;
        }
        match target_turn_id {
            Some(target_turn_id) => {
                relationship
                    .turns
                    .entry(target_turn_id)
                    .and_modify(|current| {
                        current.merge(
                            response_observation,
                            minimum_event_sequence,
                            after_item_id.clone(),
                        );
                    })
                    .or_insert_with(|| {
                        ResponseTurnObservation::new(
                            response_observation,
                            minimum_event_sequence,
                            after_item_id.clone(),
                        )
                    });
            }
            None => match pending_binding {
                ResponseObservationBinding::NextTurn => relationship
                    .pending_next_turn
                    .get_or_insert_with(ResponseTurnObservation::default)
                    .merge(response_observation, minimum_event_sequence, after_item_id),
                ResponseObservationBinding::ExplicitAdmission(admission_id) => relationship
                    .pending_admissions
                    .entry(admission_id)
                    .or_default()
                    .merge(response_observation, minimum_event_sequence, after_item_id),
            },
        }

        state
            .response_observers
            .insert(observer_child, Arc::downgrade(observer));
        let inserted = state.response_watchers.insert(observer_child);
        drop(state);
        self.publish_response_observation_binding();
        inserted.then(|| ResponseWatcherRegistration {
            state: Arc::clone(&self.wait_agent_presentations),
            parent,
            child,
            active: true,
            preserve_state: false,
        })
    }

    pub(crate) fn cancel_response_observation_admission(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        admission_id: Uuid,
    ) {
        if let Some(relationship) = self
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
        {
            relationship.pending_admissions.remove(&admission_id);
        }
        self.publish_response_observation_binding();
    }

    pub(crate) fn target_message_admission(
        &self,
        observer: SessionPresentationId,
        target: SessionPresentationId,
        target_turn_id: &str,
        observer_active_turn_id: Option<&str>,
        observer_last_terminal_turn_id: Option<&str>,
        mode: TargetMessageAdmissionMode,
    ) -> CodexResult<TargetMessageAdmission> {
        let may_steer = mode == TargetMessageAdmissionMode::SteerOrWake;
        let mut state = self.wait_agent_presentations.state();
        let observation = state
            .response_observation_by_observer_child
            .get_mut(&(observer, target))
            .filter(|relationship| !relationship.revoked)
            .and_then(|relationship| relationship.turns.get_mut(target_turn_id))
            .filter(|observation| observation.target_messages)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "agent {} has no message route to {} for turn {target_turn_id}",
                    target.thread_id, observer.thread_id
                ))
            })?;
        if observation.message_wake_reservation_id.is_some() {
            return if observer_active_turn_id.is_some() && may_steer {
                Ok(TargetMessageAdmission::Steer)
            } else {
                Ok(TargetMessageAdmission::PendingWake)
            };
        }
        match observation.message_wake_turn_id.as_deref() {
            None if observer_active_turn_id.is_some() && may_steer => {
                Ok(TargetMessageAdmission::Steer)
            }
            None => {
                let reservation_id = Uuid::now_v7();
                observation.message_wake_reservation_id = Some(reservation_id);
                Ok(TargetMessageAdmission::Wake(reservation_id))
            }
            Some(wake_turn_id) if observer_active_turn_id == Some(wake_turn_id) && may_steer => {
                Ok(TargetMessageAdmission::Steer)
            }
            Some(wake_turn_id) if observer_last_terminal_turn_id == Some(wake_turn_id) => {
                Err(CodexErr::InvalidRequest(format!(
                    "agent message route to {} already used its idle wake",
                    observer.thread_id
                )))
            }
            Some(_) if observer_active_turn_id.is_none() && may_steer => {
                Ok(TargetMessageAdmission::PendingWake)
            }
            Some(_) => Err(CodexErr::InvalidRequest(format!(
                "agent message route to {} belongs to another source turn",
                observer.thread_id
            ))),
        }
    }

    pub(crate) fn target_message_binding_pending(
        &self,
        observer: SessionPresentationId,
        target: SessionPresentationId,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(observer, target))
            .filter(|relationship| !relationship.revoked)
            .is_some_and(|relationship| {
                // Only input already being admitted can bind a route for a running sender.
                // A next-turn reservation cannot authorize that sender's current turn.
                relationship
                    .pending_admissions
                    .values()
                    .any(|observation| observation.target_messages)
            })
    }

    pub(crate) fn commit_target_message_wake(
        &self,
        observer: SessionPresentationId,
        target: SessionPresentationId,
        target_turn_id: &str,
        reservation_id: Uuid,
        wake_turn_id: &str,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(observer, target))
            .filter(|relationship| !relationship.revoked)
            .and_then(|relationship| relationship.turns.get_mut(target_turn_id))
            .filter(|observation| observation.target_messages)
        else {
            return false;
        };
        if observation.message_wake_reservation_id != Some(reservation_id) {
            return false;
        }
        observation.message_wake_reservation_id = None;
        let committed = match observation.message_wake_turn_id.as_deref() {
            None => {
                observation.message_wake_turn_id = Some(wake_turn_id.to_string());
                true
            }
            Some(existing) => existing == wake_turn_id,
        };
        drop(state);
        if committed {
            self.wait_agent_presentations
                .response_observation_changed
                .notify_waiters();
        }
        committed
    }

    pub(crate) fn rollback_target_message_wake(
        &self,
        observer: SessionPresentationId,
        target: SessionPresentationId,
        target_turn_id: &str,
        reservation_id: Uuid,
    ) {
        let mut state = self.wait_agent_presentations.state();
        if let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(observer, target))
            .and_then(|relationship| relationship.turns.get_mut(target_turn_id))
            && observation.message_wake_reservation_id == Some(reservation_id)
        {
            observation.message_wake_reservation_id = None;
        }
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
    }

    /// Retires only the grant that consumed this exact source turn's idle wake.
    pub(crate) fn finish_target_message_wake(
        &self,
        observer: SessionPresentationId,
        wake_turn_id: &str,
    ) {
        let mut changed_children = Vec::new();
        let mut state = self.wait_agent_presentations.state();
        for ((parent, child), relationship) in &mut state.response_observation_by_observer_child {
            if *parent != observer || relationship.revoked {
                continue;
            }
            let mut changed = false;
            for observation in relationship.turns.values_mut() {
                if observation.target_messages
                    && observation.message_wake_turn_id.as_deref() == Some(wake_turn_id)
                {
                    observation.target_messages = false;
                    observation.message_wake_reservation_id = None;
                    changed = true;
                }
            }
            if changed {
                changed_children.push(*child);
            }
        }
        drop(state);
        if changed_children.is_empty() {
            return;
        }
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let control = self.clone();
            runtime.spawn(async move {
                let _transaction = control
                    .acquire_response_observation_transaction(observer)
                    .await;
                for child in changed_children {
                    if let Err(error) = control
                        .persist_response_observation_snapshot(observer, child)
                        .await
                    {
                        control.abandon_response_observer(
                            observer,
                            child,
                            &format!("completed agent-message wake publication failed: {error}"),
                        );
                    }
                }
            });
        }
    }
}

fn relationship_has_bound_final_response_wake(relationship: &ResponseObserverRelationship) -> bool {
    relationship.turns.values().any(|observation| {
        observation.final_response == FinalResponseObservation::Wake
            && observation
                .final_delivery_response_item_id
                .as_ref()
                .is_none_or(|id| {
                    !observation
                        .committed_delivery_response_item_ids
                        .contains(id)
                })
    })
}

impl ResponseWatcherRegistration {
    pub(crate) fn retire_if_observation_idle(&mut self) -> bool {
        if !self.active {
            return true;
        }
        let mut state = self.state.state();
        if state
            .response_observation_by_observer_child
            .get(&(self.parent, self.child))
            .is_some_and(|relationship| {
                relationship.baseline_final_response != FinalResponseObservation::None
                    || relationship.pending_next_turn.is_some()
                    || !relationship.pending_admissions.is_empty()
                    || relationship.turns.values().any(|turn| {
                        !turn.commentary_admissions.is_empty()
                            || turn.commentary_delivery.is_some()
                            || (turn.final_response != FinalResponseObservation::None
                                && turn
                                    .final_delivery_response_item_id
                                    .as_ref()
                                    .is_none_or(|id| {
                                        !turn.committed_delivery_response_item_ids.contains(id)
                                    }))
                    })
            })
        {
            return false;
        }
        state.response_watchers.remove(&(self.parent, self.child));
        state.response_observers.remove(&(self.parent, self.child));
        self.active = false;
        true
    }

    pub(crate) fn preserve_state_for_replacement_on_drop(&mut self) {
        self.preserve_state = true;
    }
}

impl Drop for ResponseWatcherRegistration {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.state.state();
        let pair = (self.parent, self.child);
        state.response_watchers.remove(&pair);
        state.response_observers.remove(&pair);
        if !self.preserve_state {
            state.revoke_response_observation(pair);
        }
        drop(state);
        self.state.response_observation_changed.notify_waiters();
    }
}
