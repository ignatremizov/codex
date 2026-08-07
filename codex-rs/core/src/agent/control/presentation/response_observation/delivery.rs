use super::*;
use std::collections::HashSet;

enum CommentaryEventBoundary<'a> {
    Live {
        sequence: u64,
    },
    Recovered {
        sequence: u64,
        prior_item_ids: &'a HashSet<String>,
    },
}

impl AgentControl {
    #[cfg(test)]
    pub(crate) fn prepare_commentary_observation_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        source_item_id: &str,
        text: &str,
    ) -> Option<AgentResponseCommentaryDelivery> {
        self.prepare_commentary_observation_delivery_at_sequence(
            parent,
            child,
            turn_id,
            source_item_id,
            text,
            u64::MAX,
        )
    }

    pub(crate) fn prepare_commentary_observation_delivery_at_sequence(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        source_item_id: &str,
        text: &str,
        sequence: u64,
    ) -> Option<AgentResponseCommentaryDelivery> {
        self.prepare_commentary_observation_delivery_at_boundary(
            parent,
            child,
            turn_id,
            source_item_id,
            text,
            CommentaryEventBoundary::Live { sequence },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_recovered_commentary_observation_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        source_item_id: &str,
        text: &str,
        sequence: u64,
        prior_item_ids: &HashSet<String>,
    ) -> Option<AgentResponseCommentaryDelivery> {
        self.prepare_commentary_observation_delivery_at_boundary(
            parent,
            child,
            turn_id,
            source_item_id,
            text,
            CommentaryEventBoundary::Recovered {
                sequence,
                prior_item_ids,
            },
        )
    }

    fn prepare_commentary_observation_delivery_at_boundary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        source_item_id: &str,
        text: &str,
        boundary: CommentaryEventBoundary<'_>,
    ) -> Option<AgentResponseCommentaryDelivery> {
        let mut state = self.wait_agent_presentations.state();
        let observation = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))?;
        if observation.commentary_delivery.is_some() {
            return None;
        }
        let pending_before = observation.commentary_admissions.len();
        observation.commentary_admissions.retain(|admission| {
            let admitted = match &boundary {
                CommentaryEventBoundary::Live { sequence } => {
                    admission.minimum_event_sequence <= *sequence
                }
                CommentaryEventBoundary::Recovered {
                    sequence,
                    prior_item_ids,
                } => {
                    if admission.canonical_boundary {
                        admission
                            .after_item_id
                            .as_ref()
                            .is_none_or(|item_id| prior_item_ids.contains(item_id))
                    } else {
                        admission.minimum_event_sequence <= *sequence
                    }
                }
            };
            !admitted
        });
        if observation.commentary_admissions.len() == pending_before {
            return None;
        }
        let delivery = AgentResponseCommentaryDelivery {
            source_item_id: source_item_id.to_string(),
            text: text.to_string(),
            // `new` appends a fresh UUIDv7; `amsg` is only the readable item-kind prefix.
            response_item_id: ResponseItemId::new("amsg"),
        };
        observation.commentary_delivery = Some(delivery.clone());
        Some(delivery)
    }

    pub(crate) fn prepare_final_response_observation_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        response_item_id: &ResponseItemId,
    ) -> (FinalResponseObservation, Option<ResponseItemId>) {
        let mut state = self.wait_agent_presentations.state();
        let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
        else {
            return (FinalResponseObservation::None, None);
        };
        let final_response = observation.final_response;
        if matches!(
            final_response,
            FinalResponseObservation::None | FinalResponseObservation::PresentationOnly
        ) {
            return (final_response, None);
        }
        let response_item_id = observation
            .final_delivery_response_item_id
            .get_or_insert_with(|| response_item_id.clone())
            .clone();
        if observation
            .committed_delivery_response_item_ids
            .contains(&response_item_id)
        {
            return (FinalResponseObservation::None, None);
        }
        (final_response, Some(response_item_id))
    }

    pub(crate) fn response_observation_commentary_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> Option<AgentResponseCommentaryDelivery> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.turns.get(turn_id))
            .and_then(|observation| observation.commentary_delivery.clone())
    }

    pub(crate) fn pending_response_observation_commentary_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<(String, AgentResponseCommentaryDelivery)> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| {
                relationship
                    .turns
                    .iter()
                    .find_map(|(turn_id, observation)| {
                        observation
                            .commentary_delivery
                            .clone()
                            .map(|delivery| (turn_id.clone(), delivery))
                    })
            })
    }

    pub(crate) fn commit_commentary_observation_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) {
        if let Some(observation) = self
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
            && let Some(delivery) = observation.commentary_delivery.take()
        {
            observation
                .committed_delivery_response_item_ids
                .push(delivery.response_item_id);
        }
    }

    pub(crate) fn commit_final_response_observation_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) {
        if let Some(observation) = self
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
            && let Some(response_item_id) = observation.final_delivery_response_item_id.as_ref()
            && !observation
                .committed_delivery_response_item_ids
                .contains(response_item_id)
        {
            observation
                .committed_delivery_response_item_ids
                .push(response_item_id.clone());
        }
    }

    pub(crate) fn finish_response_observation_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> Vec<AgentResponseObservation> {
        let mut state = self.wait_agent_presentations.state();
        let baseline_final_delivery = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .map(|relationship| {
                relationship.turns.remove(turn_id);
                relationship.baseline_final_response.into()
            })
            .unwrap_or(codex_protocol::protocol::AgentResponseFinalDelivery::None);
        drop(state);
        let mut snapshots = vec![AgentResponseObservation {
            observer_thread_id: parent.thread_id,
            target_thread_id: child.thread_id,
            target_turn_id: Some(turn_id.to_string()),
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            baseline_final_delivery,
            final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        }];
        snapshots.extend(self.response_observation_snapshots(parent, child));
        snapshots
    }

    #[cfg(test)]
    pub(crate) fn bind_response_observation_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        binding: ResponseObservationBinding,
    ) {
        self.bind_response_observation_turn_at_sequence(
            parent,
            child,
            turn_id,
            binding,
            /*commentary_boundary*/ None,
            ResponseObservationBindingPublication::Immediate,
        );
    }

    pub(crate) fn bind_response_observation_turn_at_sequence(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        binding: ResponseObservationBinding,
        commentary_boundary: Option<(u64, Option<String>)>,
        publication: ResponseObservationBindingPublication,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        let mut pending = match binding {
            ResponseObservationBinding::NextTurn => relationship.pending_next_turn.take(),
            ResponseObservationBinding::ExplicitAdmission(admission_id) => {
                relationship.pending_admissions.remove(&admission_id)
            }
        }
        .unwrap_or_default();
        if let Some((minimum_event_sequence, after_item_id)) = commentary_boundary {
            for admission in &mut pending.commentary_admissions {
                admission.minimum_event_sequence =
                    admission.minimum_event_sequence.max(minimum_event_sequence);
                admission.after_item_id = after_item_id.clone();
                admission.canonical_boundary = true;
            }
        }
        relationship
            .turns
            .entry(turn_id.to_string())
            .and_modify(|current| {
                current
                    .commentary_admissions
                    .extend(pending.commentary_admissions.iter().cloned());
                if current.commentary_delivery.is_none() {
                    current.commentary_delivery = pending.commentary_delivery.clone();
                }
                current.final_response = current.final_response.max(pending.final_response);
                if current.final_delivery_response_item_id.is_none() {
                    current.final_delivery_response_item_id =
                        pending.final_delivery_response_item_id.clone();
                }
                for response_item_id in &pending.committed_delivery_response_item_ids {
                    if !current
                        .committed_delivery_response_item_ids
                        .contains(response_item_id)
                    {
                        current
                            .committed_delivery_response_item_ids
                            .push(response_item_id.clone());
                    }
                }
            })
            .or_insert_with(|| {
                // A non-empty pending observation already contains the exact policy requested for
                // this turn. The retained passive baseline applies only when a later turn starts
                // without a pending send/spawn/resume observation to bind.
                if pending == ResponseTurnObservation::default() {
                    ResponseTurnObservation {
                        final_response: relationship.baseline_final_response,
                        ..Default::default()
                    }
                } else {
                    pending
                }
            });
        drop(state);
        if publication == ResponseObservationBindingPublication::Immediate {
            self.publish_response_observation_binding();
        }
    }

    pub(crate) fn publish_response_observation_binding(&self) {
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
    }

    pub(crate) fn response_observation_event_match(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> ResponseObservationEventMatch {
        let state = self.wait_agent_presentations.state();
        let Some(relationship) = state
            .response_observation_by_observer_child
            .get(&(parent, child))
        else {
            return ResponseObservationEventMatch::Ignore;
        };
        if relationship.turns.contains_key(turn_id) {
            ResponseObservationEventMatch::Observe
        } else if !relationship.pending_admissions.is_empty() {
            ResponseObservationEventMatch::AwaitBinding
        } else {
            ResponseObservationEventMatch::Ignore
        }
    }

    #[cfg(test)]
    pub(crate) fn bind_response_observation_started_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> bool {
        self.bind_response_observation_started_turn_at_sequence(
            parent, child, turn_id, /*sequence*/ 0,
        )
    }

    pub(crate) fn bind_response_observation_started_turn_at_sequence(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        sequence: u64,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let Some(relationship) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
        else {
            return false;
        };
        if relationship.pending_next_turn.is_some() {
            drop(state);
            self.bind_response_observation_turn_at_sequence(
                parent,
                child,
                turn_id,
                ResponseObservationBinding::NextTurn,
                Some((sequence.saturating_add(1), None)),
                ResponseObservationBindingPublication::Immediate,
            );
            return true;
        }
        if relationship.baseline_final_response == FinalResponseObservation::None {
            return false;
        }
        relationship
            .turns
            .entry(turn_id.to_string())
            .or_insert(ResponseTurnObservation {
                final_response: relationship.baseline_final_response,
                ..Default::default()
            });
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        true
    }

    pub(crate) async fn await_response_observation_event_match(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> bool {
        loop {
            let changed = self
                .wait_agent_presentations
                .response_observation_changed
                .notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.response_observation_event_match(parent, child, turn_id) {
                ResponseObservationEventMatch::Observe => return true,
                ResponseObservationEventMatch::Ignore => return false,
                ResponseObservationEventMatch::AwaitBinding => changed.await,
            }
        }
    }
}
