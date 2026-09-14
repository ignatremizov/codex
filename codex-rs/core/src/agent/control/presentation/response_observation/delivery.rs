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

pub(crate) struct ResponseObservationTurnBinding<'a> {
    pub(crate) parent: SessionPresentationId,
    pub(crate) child: SessionPresentationId,
    pub(crate) turn_id: &'a str,
    pub(crate) binding: ResponseObservationBinding,
    pub(crate) commentary_boundary: Option<(u64, Option<String>)>,
    pub(crate) task_preview: Option<String>,
    pub(crate) publication: ResponseObservationBindingPublication,
}

impl AgentControl {
    pub(crate) fn has_mailbox_final_subscription(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .is_some_and(|relationship| {
                relationship.mailbox_final_subscription_message_id.is_some()
            })
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

    pub(crate) fn mailbox_final_subscription_child(
        &self,
        parent: SessionPresentationId,
        child_thread_id: ThreadId,
        current_child: Option<SessionPresentationId>,
    ) -> SessionPresentationId {
        current_child
            .or_else(|| {
                self.wait_agent_presentations
                    .state()
                    .response_observation_by_observer_child
                    .keys()
                    .find_map(|(observer, child)| {
                        (*observer == parent && child.thread_id == child_thread_id)
                            .then_some(*child)
                    })
            })
            .unwrap_or_else(|| SessionPresentationId::new(child_thread_id, Uuid::nil()))
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
        relationship.persistence = ResponseObservationPersistence::Durable;
        relationship.mailbox_final_subscription_message_id = None;
        relationship.mailbox_final_subscription_suppressed_message_id =
            Some(message_id.to_string());
        for observation in relationship
            .pending_next_turn
            .iter_mut()
            .chain(relationship.pending_admissions.values_mut())
        {
            if observation.mailbox_final_subscription_message_id.is_some() {
                observation.mailbox_final_subscription_message_id = None;
                if observation.final_delivery_response_item_id.is_none() {
                    observation.final_response = FinalResponseObservation::None;
                }
            }
        }
        for (turn_id, observation) in &mut relationship.turns {
            if observation.mailbox_final_subscription_message_id.is_some() {
                observation.mailbox_final_subscription_message_id = None;
                if observation.final_delivery_response_item_id.is_none()
                    && preserved_turn_id != Some(turn_id.as_str())
                {
                    observation.final_response = FinalResponseObservation::None;
                }
            }
        }
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
        observation.commentary_delivery_route = CommentaryDeliveryRoute::Undecided;
        Some(delivery)
    }

    pub(crate) fn prepare_final_response_observation_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        response_item_id: &ResponseItemId,
    ) -> (
        FinalResponseObservation,
        Option<ResponseItemId>,
        bool,
        Option<String>,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
        else {
            return (FinalResponseObservation::None, None, false, None);
        };
        let final_response = observation.final_response;
        let queue_delivery = observation.queue_delivery;
        if matches!(
            final_response,
            FinalResponseObservation::None | FinalResponseObservation::PresentationOnly
        ) {
            return (final_response, None, queue_delivery, None);
        }
        let mailbox_final_subscription_message_id =
            observation.mailbox_final_subscription_message_id.clone();
        let response_item_id = observation
            .final_delivery_response_item_id
            .get_or_insert_with(|| response_item_id.clone())
            .clone();
        if observation
            .committed_delivery_response_item_ids
            .contains(&response_item_id)
        {
            return (FinalResponseObservation::None, None, queue_delivery, None);
        }
        (
            final_response,
            Some(response_item_id),
            queue_delivery,
            mailbox_final_subscription_message_id,
        )
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

    pub(in crate::agent::control) fn route_response_observer_commentary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> CommentaryDeliveryRoute {
        let mut state = self.wait_agent_presentations.state();
        let wait_active = state
            .active_targeted_waits
            .get(&(parent, child.thread_id))
            .is_some_and(|waits| !waits.is_empty())
            || state
                .active_any_child_waits
                .get(&parent)
                .is_some_and(|waits| !waits.is_empty())
            || state
                .wait_commentary_turns
                .contains(&(parent, child, turn_id.to_string()));
        let route = if wait_active {
            CommentaryDeliveryRoute::Wait
        } else {
            CommentaryDeliveryRoute::Mailbox
        };
        if let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
            && observation.commentary_delivery.is_some()
            && observation.commentary_delivery_route == CommentaryDeliveryRoute::Undecided
        {
            observation.commentary_delivery_route = route;
        }
        let route = state
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.turns.get(turn_id))
            .map_or(CommentaryDeliveryRoute::Mailbox, |observation| {
                observation.commentary_delivery_route
            });
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        route
    }

    pub(crate) fn mark_response_observer_terminal_processed(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) {
        self.wait_agent_presentations
            .state()
            .response_observer_terminal_turns
            .insert((parent, child, turn_id.to_string()));
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
    }

    /// Claims subscribed commentary from target turns whose response watcher selected the active
    /// wait as its delivery boundary.
    pub(crate) async fn wait_commentary_before_terminal(
        &self,
        parent: SessionPresentationId,
        target_turns: &[ClaimedTargetTurn],
    ) -> Vec<WaitCommentaryDelivery> {
        loop {
            let changed = self
                .wait_agent_presentations
                .response_observation_changed
                .notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (commentary, commentary_pending) = {
                let state = self.wait_agent_presentations.state();
                let mut commentary = Vec::new();
                let mut pending = false;
                for target_turn in target_turns {
                    let Some(observation) = state
                        .response_observation_by_observer_child
                        .get(&(parent, target_turn.child))
                        .and_then(|relationship| relationship.turns.get(&target_turn.turn_id))
                    else {
                        continue;
                    };
                    if observation.commentary_delivery_route == CommentaryDeliveryRoute::Wait
                        && let Some(delivery) = observation.commentary_delivery.clone()
                    {
                        commentary.push(WaitCommentaryDelivery {
                            child: target_turn.child,
                            turn_id: target_turn.turn_id.clone(),
                            delivery,
                        });
                    } else if (!observation.commentary_admissions.is_empty()
                        || (observation.commentary_delivery.is_some()
                            && observation.commentary_delivery_route
                                == CommentaryDeliveryRoute::Undecided))
                        && !state.response_observer_terminal_turns.contains(&(
                            parent,
                            target_turn.child,
                            target_turn.turn_id.clone(),
                        ))
                    {
                        pending = true;
                    }
                }
                (commentary, pending)
            };
            if !commentary_pending {
                return commentary;
            }
            changed.as_mut().await;
        }
    }

    pub(crate) fn release_wait_commentary_delivery(
        &self,
        parent: SessionPresentationId,
        commentary: &WaitCommentaryDelivery,
    ) {
        let mut state = self.wait_agent_presentations.state();
        state
            .wait_commentary_turns
            .remove(&(parent, commentary.child, commentary.turn_id.clone()));
        if let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, commentary.child))
            .and_then(|relationship| relationship.turns.get_mut(&commentary.turn_id))
            && observation
                .commentary_delivery
                .as_ref()
                .is_some_and(|delivery| {
                    delivery.response_item_id == commentary.delivery.response_item_id
                })
            && observation.commentary_delivery_route == CommentaryDeliveryRoute::Wait
        {
            observation.commentary_delivery_route = CommentaryDeliveryRoute::Mailbox;
        }
        drop(state);
        self.wait_agent_presentations
            .response_observation_changed
            .notify_waiters();
        self.wait_agent_presentations
            .watcher_terminal_changed
            .notify_waiters();
    }

    pub(crate) fn commit_response_observation_delivery(
        &self,
        commit: &ResponseObservationDeliveryCommit,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let Some(relationship) = state
            .response_observation_by_observer_child
            .get_mut(&(commit.parent, commit.child))
        else {
            return;
        };
        let Some(observation) = relationship.turns.get_mut(&commit.turn_id) else {
            if commit.kind == ResponseObservationDeliveryKind::Final
                && let Some(message_id) = commit.mailbox_final_subscription_message_id.as_ref()
                && relationship.mailbox_final_subscription_message_id.as_ref() == Some(message_id)
            {
                relationship.mailbox_final_subscription_message_id = None;
                drop(state);
                self.publish_response_observation_binding();
            }
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
                observation.commentary_delivery_route = CommentaryDeliveryRoute::Undecided;
            }
            ResponseObservationDeliveryKind::Final => {
                if observation.final_delivery_response_item_id.as_ref()
                    != Some(&commit.response_item_id)
                {
                    return;
                }
                if commit.mailbox_final_subscription_message_id.as_ref()
                    == observation.mailbox_final_subscription_message_id.as_ref()
                    && relationship.mailbox_final_subscription_message_id.as_ref()
                        == commit.mailbox_final_subscription_message_id.as_ref()
                {
                    relationship.mailbox_final_subscription_message_id = None;
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
            task_preview: None,
            promoted_task_context: None,
            pending_commentary: false,
            commentary_after_sequences: Vec::new(),
            commentary_admissions: Vec::new(),
            commentary_delivery: None,
            target_messages: false,
            reply_route_enabled: None,
            reply_route_context_installed: false,
            queue_delivery: false,
            message_wake_turn_id: None,
            baseline_final_delivery,
            final_delivery: codex_protocol::protocol::AgentResponseFinalDelivery::None,
            final_delivery_response_item_id: None,
            committed_delivery_response_item_ids: Vec::new(),
            mailbox_final_subscription_message_id: None,
            mailbox_final_subscription_suppressed_message_id: None,
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
        self.bind_response_observation_turn_at_sequence(ResponseObservationTurnBinding {
            parent,
            child,
            turn_id,
            binding,
            commentary_boundary: None,
            task_preview: None,
            publication: ResponseObservationBindingPublication::Immediate,
        });
    }

    pub(crate) fn bind_response_observation_turn_at_sequence(
        &self,
        binding: ResponseObservationTurnBinding<'_>,
    ) {
        let ResponseObservationTurnBinding {
            parent,
            child,
            turn_id,
            binding,
            commentary_boundary,
            task_preview,
            publication,
        } = binding;
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
        let task_preview = compact_task_preview(task_preview);
        if task_preview.is_some() {
            pending.task_preview = task_preview;
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
                    current.commentary_delivery_route = pending.commentary_delivery_route;
                }
                if pending.task_preview.is_some() {
                    current.task_preview = pending.task_preview.clone();
                }
                current.final_response = current.final_response.max(pending.final_response);
                current.target_messages |= pending.target_messages;
                current.queue_delivery |= pending.queue_delivery;
                if current.message_wake_turn_id.is_none() {
                    current.message_wake_turn_id = pending.message_wake_turn_id.clone();
                }
                if pending.response_observation_selection_id.is_some() {
                    current.response_observation_selection_id =
                        pending.response_observation_selection_id;
                }
                if current.mailbox_final_subscription_message_id.is_none() {
                    current.mailbox_final_subscription_message_id =
                        pending.mailbox_final_subscription_message_id.clone();
                }
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

    pub(crate) fn set_response_observation_task_preview(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        target_turn_id: Option<&str>,
        task_preview: String,
    ) {
        let Some(task_preview) = compact_task_preview(Some(task_preview)) else {
            return;
        };
        let mut state = self.wait_agent_presentations.state();
        let Some(relationship) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
        else {
            return;
        };
        let observation = match target_turn_id {
            Some(target_turn_id) => relationship.turns.get_mut(target_turn_id),
            None => relationship.pending_next_turn.as_mut(),
        };
        if let Some(observation) = observation {
            observation.task_preview = Some(task_preview);
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
            self.bind_response_observation_turn_at_sequence(ResponseObservationTurnBinding {
                parent,
                child,
                turn_id,
                binding: ResponseObservationBinding::NextTurn,
                commentary_boundary: Some((sequence.saturating_add(1), None)),
                task_preview: None,
                publication: ResponseObservationBindingPublication::Immediate,
            });
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

#[cfg(test)]
#[path = "mailbox_final_subscription_retirement_tests.rs"]
mod tests;
