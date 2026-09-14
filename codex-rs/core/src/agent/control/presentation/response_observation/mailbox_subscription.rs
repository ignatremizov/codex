//! Runtime projection of durable mailbox tokens. History never grants subscription authority.

use super::*;

fn retire_mailbox_final_subscription_from_observation(
    observation: &mut ResponseTurnObservation,
    message_id: &str,
    selected_selection_id: Uuid,
) {
    let is_selected = observation.response_observation_selection_id == Some(selected_selection_id);
    let owns_subscription =
        observation.mailbox_final_subscription_message_id.as_deref() == Some(message_id);
    if owns_subscription
        && !is_selected
        && observation.final_delivery_response_item_id.is_none()
        && observation.committed_delivery_response_item_ids.is_empty()
    {
        observation.final_response = FinalResponseObservation::None;
    }
    if owns_subscription {
        observation.mailbox_final_subscription_message_id = None;
    }
    if is_selected {
        observation.response_observation_selection_id = None;
    }
}

pub(super) fn suppress_mailbox_subscription(
    relationship: &mut ResponseObserverRelationship,
    message_id: &str,
    preserved_turn_id: Option<&str>,
) {
    relationship.persistence = ResponseObservationPersistence::Durable;
    if relationship
        .mailbox_final_subscription_message_id
        .as_deref()
        == Some(message_id)
    {
        relationship.mailbox_final_subscription_message_id = None;
    }
    relationship.mailbox_final_subscription_suppressed_message_id = Some(message_id.to_string());
    for observation in relationship
        .pending_next_turn
        .iter_mut()
        .chain(relationship.pending_admissions.values_mut())
    {
        if observation.mailbox_final_subscription_message_id.as_deref() == Some(message_id) {
            observation.mailbox_final_subscription_message_id = None;
            if observation.final_delivery_response_item_id.is_none() {
                observation.final_response = FinalResponseObservation::None;
            }
        }
    }
    for (turn_id, observation) in &mut relationship.turns {
        if observation.mailbox_final_subscription_message_id.as_deref() == Some(message_id) {
            observation.mailbox_final_subscription_message_id = None;
            if observation.final_delivery_response_item_id.is_none()
                && preserved_turn_id != Some(turn_id.as_str())
            {
                observation.final_response = FinalResponseObservation::None;
            }
        }
    }
}

impl LocalAgentControl {
    pub(crate) fn mailbox_final_subscription_for_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> Option<String> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.turns.get(turn_id))
            .and_then(|observation| observation.mailbox_final_subscription_message_id.clone())
    }

    pub(crate) fn mailbox_final_subscription_message_id(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<String> {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.mailbox_final_subscription_message_id.clone())
    }

    pub(crate) fn mailbox_final_subscription_was_suppressed(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        message_id: &str,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .is_some_and(|relationship| {
                relationship
                    .mailbox_final_subscription_suppressed_message_id
                    .as_deref()
                    == Some(message_id)
            })
    }

    pub(crate) fn install_mailbox_final_subscription(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        message_id: &str,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        if relationship
            .mailbox_final_subscription_suppressed_message_id
            .as_deref()
            == Some(message_id)
        {
            return false;
        }
        relationship.persistence = ResponseObservationPersistence::Durable;
        relationship.baseline_final_response = FinalResponseObservation::None;
        relationship.mailbox_final_subscription_suppressed_message_id = None;
        for observation in relationship
            .pending_next_turn
            .iter_mut()
            .chain(relationship.pending_admissions.values_mut())
            .chain(relationship.turns.values_mut())
        {
            if observation.final_delivery_response_item_id.is_none()
                && observation.final_response != FinalResponseObservation::PresentationOnly
            {
                observation.final_response = FinalResponseObservation::None;
                observation.mailbox_final_subscription_message_id = None;
            }
        }
        relationship.mailbox_final_subscription_message_id = Some(message_id.to_string());
        drop(state);
        self.publish_response_observation_binding();
        true
    }

    pub(crate) fn bind_mailbox_final_subscription_to_turn(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        message_id: &str,
        turn_id: &str,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        if relationship
            .mailbox_final_subscription_message_id
            .as_deref()
            != Some(message_id)
        {
            return false;
        }
        let observation = relationship.turns.entry(turn_id.to_string()).or_default();
        if observation.final_delivery_response_item_id.is_some() {
            return false;
        }
        observation.final_response = FinalResponseObservation::Wake;
        observation.mailbox_final_subscription_message_id = Some(message_id.to_string());
        drop(state);
        self.publish_response_observation_binding();
        true
    }

    pub(crate) fn clear_mailbox_final_subscription(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        message_id: &str,
        preserved_turn_id: Option<&str>,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        suppress_mailbox_subscription(relationship, message_id, preserved_turn_id);
        drop(state);
        self.publish_response_observation_binding();
        true
    }

    pub(crate) fn retire_mailbox_final_subscription_preserving_observation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        message_id: &str,
        selected: &ResponseObservationSelection,
    ) -> bool {
        let mut state = self.wait_agent_presentations.state();
        let relationship = state
            .response_observation_by_observer_child
            .entry((parent, child))
            .or_default();
        relationship.persistence = ResponseObservationPersistence::Durable;
        if relationship
            .mailbox_final_subscription_message_id
            .as_deref()
            == Some(message_id)
        {
            relationship.mailbox_final_subscription_message_id = None;
        }
        relationship.mailbox_final_subscription_suppressed_message_id =
            Some(message_id.to_string());
        if let Some(observation) = relationship.pending_next_turn.as_mut() {
            retire_mailbox_final_subscription_from_observation(
                observation,
                message_id,
                selected.selection_id,
            );
        }
        for observation in relationship.pending_admissions.values_mut() {
            retire_mailbox_final_subscription_from_observation(
                observation,
                message_id,
                selected.selection_id,
            );
        }
        for observation in relationship.turns.values_mut() {
            retire_mailbox_final_subscription_from_observation(
                observation,
                message_id,
                selected.selection_id,
            );
        }
        drop(state);
        self.publish_response_observation_binding();
        true
    }
}

#[cfg(test)]
#[path = "mailbox_final_subscription_retirement_tests.rs"]
mod tests;
