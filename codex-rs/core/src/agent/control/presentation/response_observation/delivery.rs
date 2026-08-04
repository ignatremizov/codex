use super::*;

impl LocalAgentControl {
    pub(crate) fn prepare_commentary_observation_delivery_at_sequence(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        source_item_id: &str,
        text: &str,
        sequence: u64,
    ) -> Option<AgentResponseCommentaryDelivery> {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))?;
        if relationship.revoked {
            return None;
        }
        let observation = relationship.turns.get_mut(turn_id)?;
        if observation.commentary_delivery.is_some() {
            return None;
        }
        let pending_before = observation.commentary_admissions.len();
        observation
            .commentary_admissions
            .retain(|admission| admission.minimum_event_sequence > sequence);
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
        if final_response == FinalResponseObservation::None {
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

    pub(crate) fn commit_response_observation_delivery(
        &self,
        commit: &ResponseObservationDeliveryCommit,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(commit.parent, commit.child))
            .and_then(|relationship| relationship.turns.get_mut(&commit.turn_id))
        else {
            return;
        };
        match commit.kind {
            ResponseObservationDeliveryKind::Commentary => {
                let Some(delivery) = observation.commentary_delivery.as_ref() else {
                    return;
                };
                if delivery.response_item_id != commit.response_item_id {
                    return;
                }
                observation.commentary_delivery = None;
            }
            ResponseObservationDeliveryKind::Final => {
                if observation.final_delivery_response_item_id.as_ref()
                    != Some(&commit.response_item_id)
                {
                    return;
                }
            }
        }
        if !observation
            .committed_delivery_response_item_ids
            .contains(&commit.response_item_id)
        {
            observation
                .committed_delivery_response_item_ids
                .push(commit.response_item_id.clone());
        }
    }

    pub(crate) fn finish_response_observation_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> Vec<AgentResponseObservation> {
        let mut state = self.wait_agent_presentations.state();
        if let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
        {
            // Retire live policy, not accepted evidence. A pending delivery still owns its receipt.
            observation.commentary_admissions.clear();
            observation.final_response = FinalResponseObservation::None;
        }
        drop(state);
        self.response_observation_snapshots(parent, child)
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
        let Some(relationship) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
        else {
            return;
        };
        if relationship.revoked {
            return;
        }
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
        if relationship.revoked {
            return false;
        }
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
}
