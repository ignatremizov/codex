use super::*;

impl AgentControl {
    pub(crate) fn response_observation_snapshots(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Vec<AgentResponseObservation> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .filter(|relationship| {
                relationship.persistence == ResponseObservationPersistence::Durable
            })
            .map(|relationship| {
                let mut snapshots = Vec::with_capacity(relationship.turns.len().saturating_add(1));
                let empty_pending = ResponseTurnObservation::default();
                let pending = relationship
                    .pending_next_turn
                    .as_ref()
                    .unwrap_or(&empty_pending);
                snapshots.push(AgentResponseObservation {
                    observer_thread_id: parent.thread_id,
                    target_thread_id: child.thread_id,
                    target_turn_id: None,
                    pending_commentary: !pending.commentary_admissions.is_empty(),
                    commentary_after_sequences: Vec::new(),
                    commentary_admissions: pending.commentary_admissions.clone(),
                    commentary_delivery: pending.commentary_delivery.clone(),
                    baseline_final_delivery: relationship.baseline_final_response.into(),
                    final_delivery: pending.final_response.into(),
                    final_delivery_response_item_id: pending
                        .final_delivery_response_item_id
                        .clone(),
                    committed_delivery_response_item_ids: pending
                        .committed_delivery_response_item_ids
                        .clone(),
                });
                let mut turns = relationship.turns.iter().collect::<Vec<_>>();
                turns.sort_by_key(|(turn_id, _)| *turn_id);
                snapshots.extend(turns.into_iter().map(|(turn_id, observation)| {
                    AgentResponseObservation {
                        observer_thread_id: parent.thread_id,
                        target_thread_id: child.thread_id,
                        target_turn_id: Some(turn_id.clone()),
                        pending_commentary: !observation.commentary_admissions.is_empty(),
                        commentary_after_sequences: Vec::new(),
                        commentary_admissions: observation.commentary_admissions.clone(),
                        commentary_delivery: observation.commentary_delivery.clone(),
                        baseline_final_delivery: relationship.baseline_final_response.into(),
                        final_delivery: observation.final_response.into(),
                        final_delivery_response_item_id: observation
                            .final_delivery_response_item_id
                            .clone(),
                        committed_delivery_response_item_ids: observation
                            .committed_delivery_response_item_ids
                            .clone(),
                    }
                }));
                snapshots
            })
            .unwrap_or_default()
    }

    pub(crate) fn response_observation_snapshots_for_parent(
        &self,
        parent: SessionPresentationId,
    ) -> Vec<AgentResponseObservation> {
        let mut children = self
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .keys()
            .filter_map(|(observer, child)| (*observer == parent).then_some(*child))
            .collect::<Vec<_>>();
        children.sort_by_key(|child| (child.thread_id.to_string(), child.instance_id));
        children
            .into_iter()
            .flat_map(|child| self.response_observation_snapshots(parent, child))
            .collect()
    }

    pub(crate) fn response_observation_audit_snapshots(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        target_turn_id: Option<String>,
    ) -> Vec<AgentResponseObservation> {
        let mut snapshots = self.response_observation_snapshots(parent, child);
        if snapshots
            .iter()
            .any(|observation| observation.target_turn_id == target_turn_id)
        {
            return snapshots;
        }
        snapshots.push(AgentResponseObservation {
            observer_thread_id: parent.thread_id,
            target_thread_id: child.thread_id,
            target_turn_id,
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            baseline_final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
        });
        snapshots
    }

    pub(crate) fn response_observation_committed_snapshots(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        response_item_id: &ResponseItemId,
        delivery_kind: ResponseObservationDeliveryKind,
    ) -> Vec<AgentResponseObservation> {
        let mut snapshots = self.response_observation_snapshots(parent, child);
        let Some(observation) = snapshots
            .iter_mut()
            .find(|observation| observation.target_turn_id.as_deref() == Some(turn_id))
        else {
            return Vec::new();
        };
        let matches_delivery = match delivery_kind {
            ResponseObservationDeliveryKind::Commentary => observation
                .commentary_delivery
                .as_ref()
                .is_some_and(|delivery| delivery.response_item_id == *response_item_id),
            ResponseObservationDeliveryKind::Final => observation
                .final_delivery_response_item_id
                .as_ref()
                .is_some_and(|delivery_id| delivery_id == response_item_id),
        };
        if !matches_delivery {
            return Vec::new();
        }
        if delivery_kind == ResponseObservationDeliveryKind::Commentary {
            observation.commentary_delivery = None;
        }
        if !observation
            .committed_delivery_response_item_ids
            .contains(response_item_id)
        {
            observation
                .committed_delivery_response_item_ids
                .push(response_item_id.clone());
        }
        snapshots
    }

    pub(crate) fn wait_response_observation_committed_snapshots(
        &self,
        parent: SessionPresentationId,
        claimed_target_turns: &[ClaimedTargetTurn],
    ) -> Vec<AgentResponseObservation> {
        claimed_target_turns
            .iter()
            .flat_map(|target| {
                let (_final_response, response_item_id) = self
                    .prepare_final_response_observation_delivery(
                        parent,
                        target.child,
                        &target.turn_id,
                        &target.response_item_id,
                    );
                response_item_id
                    .map(|response_item_id| {
                        self.response_observation_committed_snapshots(
                            parent,
                            target.child,
                            &target.turn_id,
                            &response_item_id,
                            ResponseObservationDeliveryKind::Final,
                        )
                    })
                    .unwrap_or_default()
            })
            .collect()
    }
}
