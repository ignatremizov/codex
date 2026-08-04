use super::*;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use codex_protocol::protocol::AgentResponseCommentaryAdmission;
use codex_protocol::protocol::AgentResponseCommentaryDelivery;
use codex_protocol::protocol::AgentResponseObservation;

mod delivery;
mod snapshot;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ResponseTurnObservation {
    pub(super) commentary_admissions: Vec<AgentResponseCommentaryAdmission>,
    pub(super) commentary_delivery: Option<AgentResponseCommentaryDelivery>,
    pub(super) final_response: FinalResponseObservation,
    pub(super) final_delivery_response_item_id: Option<ResponseItemId>,
    pub(super) committed_delivery_response_item_ids: Vec<ResponseItemId>,
}

impl Default for ResponseTurnObservation {
    fn default() -> Self {
        Self {
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            final_response: FinalResponseObservation::None,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        }
    }
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
    }
}

#[derive(Clone)]
pub(in crate::agent::control) struct ResponseObserverRelationship {
    pub(super) persistence: ResponseObservationPersistence,
    pub(super) baseline_final_response: FinalResponseObservation,
    pub(super) pending_next_turn: Option<ResponseTurnObservation>,
    pub(super) pending_admissions: HashMap<Uuid, ResponseTurnObservation>,
    pub(super) turns: HashMap<String, ResponseTurnObservation>,
}

impl Default for ResponseObserverRelationship {
    fn default() -> Self {
        Self {
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

enum CompletionDeliveryAdmissionRegistration {
    #[cfg(test)]
    Untracked,
    Tracked(Weak<SubmissionAdmission>),
}

impl AgentControl {
    pub(in crate::agent::control) fn response_observation_relationship_snapshot(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<ResponseObserverRelationship> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .cloned()
    }

    pub(in crate::agent::control) fn restore_response_observation_relationship_snapshot(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        relationship: Option<ResponseObserverRelationship>,
    ) {
        let mut state = self.wait_agent_presentations.state();
        match relationship {
            Some(relationship) => {
                state
                    .response_observation_by_observer_child
                    .insert((parent, child), relationship);
            }
            None => {
                state
                    .response_observation_by_observer_child
                    .remove(&(parent, child));
            }
        }
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
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
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ true,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
            /*minimum_event_sequence*/ 0,
            /*after_item_id*/ None,
        )
    }

    #[cfg(test)]
    pub(crate) fn register_completion_watcher_with_admission(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: &Arc<SubmissionAdmission>,
    ) -> Option<CompletionWatcherRegistration> {
        self.register_response_watcher_with_admission(
            child,
            parent,
            admission,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ true,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
    }

    pub(crate) fn register_response_watcher_with_admission(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: &Arc<SubmissionAdmission>,
        response_observation: ResponseObservationPolicy,
        retain_passive_completion_relationship: bool,
        target_turn_id: Option<String>,
        pending_binding: ResponseObservationBinding,
        persistence: ResponseObservationPersistence,
    ) -> Option<CompletionWatcherRegistration> {
        self.register_response_watcher_with_admission_at_sequence(
            child,
            parent,
            admission,
            response_observation,
            retain_passive_completion_relationship,
            target_turn_id,
            pending_binding,
            persistence,
            /*minimum_event_sequence*/ 0,
            /*after_item_id*/ None,
        )
    }

    pub(crate) fn register_response_watcher_with_admission_at_sequence(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: &Arc<SubmissionAdmission>,
        response_observation: ResponseObservationPolicy,
        retain_passive_completion_relationship: bool,
        target_turn_id: Option<String>,
        pending_binding: ResponseObservationBinding,
        persistence: ResponseObservationPersistence,
        minimum_event_sequence: u64,
        after_item_id: Option<String>,
    ) -> Option<CompletionWatcherRegistration> {
        self.register_completion_watcher_inner(
            child,
            parent,
            CompletionDeliveryAdmissionRegistration::Tracked(Arc::downgrade(admission)),
            response_observation,
            retain_passive_completion_relationship,
            target_turn_id,
            pending_binding,
            persistence,
            minimum_event_sequence,
            after_item_id,
        )
    }

    pub(crate) fn restore_response_watcher_with_admission(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: &Arc<SubmissionAdmission>,
        observation: &AgentResponseObservation,
    ) -> Option<CompletionWatcherRegistration> {
        let policy = ResponseObservationPolicy::from_parts(
            observation.pending_commentary
                || !observation.commentary_after_sequences.is_empty()
                || !observation.commentary_admissions.is_empty(),
            observation.final_delivery.into(),
        );
        let binding = ResponseObservationBinding::NextTurn;
        let registration = self.register_response_watcher_with_admission(
            child,
            parent,
            admission,
            policy,
            observation.baseline_final_delivery
                != codex_protocol::protocol::AgentResponseFinalDelivery::None,
            observation.target_turn_id.clone(),
            binding,
            ResponseObservationPersistence::Durable,
        );
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        relationship.baseline_final_response = observation.baseline_final_delivery.into();
        let turn_observation = match observation.target_turn_id.as_deref() {
            Some(turn_id) => relationship.turns.get_mut(turn_id),
            None => match binding {
                ResponseObservationBinding::NextTurn => relationship.pending_next_turn.as_mut(),
                ResponseObservationBinding::ExplicitAdmission(admission_id) => {
                    relationship.pending_admissions.get_mut(&admission_id)
                }
            },
        };
        if let Some(turn_observation) = turn_observation {
            if !observation.commentary_admissions.is_empty() {
                turn_observation.commentary_admissions = observation.commentary_admissions.clone();
            } else if !observation.commentary_after_sequences.is_empty() {
                turn_observation.commentary_admissions = observation
                    .commentary_after_sequences
                    .iter()
                    .map(|minimum_event_sequence| AgentResponseCommentaryAdmission {
                        minimum_event_sequence: *minimum_event_sequence,
                        after_item_id: None,
                        canonical_boundary: false,
                    })
                    .collect();
            }
            turn_observation.commentary_delivery = observation.commentary_delivery.clone();
            turn_observation.final_delivery_response_item_id =
                observation.final_delivery_response_item_id.clone();
            turn_observation.committed_delivery_response_item_ids =
                observation.committed_delivery_response_item_ids.clone();
        }
        registration
    }

    fn register_completion_watcher_inner(
        &self,
        child: SessionPresentationId,
        parent: SessionPresentationId,
        admission: CompletionDeliveryAdmissionRegistration,
        response_observation: ResponseObservationPolicy,
        retain_passive_completion_relationship: bool,
        target_turn_id: Option<String>,
        pending_binding: ResponseObservationBinding,
        persistence: ResponseObservationPersistence,
        minimum_event_sequence: u64,
        after_item_id: Option<String>,
    ) -> Option<CompletionWatcherRegistration> {
        let mut state = self.wait_agent_presentations.state();
        let observer_child = (parent, child);
        let relationship = state
            .response_observation_by_observer_child
            .entry(observer_child)
            .or_default();
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
                    .merge(
                        response_observation,
                        minimum_event_sequence,
                        after_item_id.clone(),
                    ),
                ResponseObservationBinding::ExplicitAdmission(admission_id) => relationship
                    .pending_admissions
                    .entry(admission_id)
                    .or_default()
                    .merge(response_observation, minimum_event_sequence, after_item_id),
            },
        }
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        let mut state = self.wait_agent_presentations.state();
        if !state.completion_watcher_sessions.insert(observer_child) {
            return None;
        }
        let parents = state
            .completion_observers_by_child
            .entry(child)
            .or_default();
        parents.retain(|existing| {
            existing.thread_id != parent.thread_id || existing.instance_id == parent.instance_id
        });
        parents.insert(parent);
        match admission {
            #[cfg(test)]
            CompletionDeliveryAdmissionRegistration::Untracked => {}
            CompletionDeliveryAdmissionRegistration::Tracked(admission) => {
                state.completion_delivery_admission_by_child.insert(
                    observer_child,
                    CompletionDeliveryAdmission { parent, admission },
                );
            }
        }
        Some(CompletionWatcherRegistration {
            presentations: Arc::clone(&self.wait_agent_presentations),
            child,
            parent,
            child_lifecycle_generation: self.agent_lifecycle_generation(child.thread_id),
            preserve_state_for_replacement_on_drop: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn cancel_response_observation_admission(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        admission_id: Uuid,
    ) {
        let mut state = self.wait_agent_presentations.state();
        if let Some(relationship) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
        {
            relationship.pending_admissions.remove(&admission_id);
        }
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
    }

    pub(crate) fn clear_response_observation_relationship(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .remove(&(parent, child));
    }
}
