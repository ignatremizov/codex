use super::*;

impl LocalAgentControl {
    pub(crate) fn route_response_observer_commentary(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> CommentaryDeliveryRoute {
        let mut state = self.wait_agent_presentations.state();
        let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
            .and_then(|relationship| relationship.turns.get_mut(turn_id))
        else {
            return CommentaryDeliveryRoute::Mailbox;
        };
        let changed = observation.commentary_delivery_route == CommentaryDeliveryRoute::Undecided;
        if changed {
            observation.commentary_delivery_route = CommentaryDeliveryRoute::Mailbox;
        }
        let route = observation.commentary_delivery_route;
        drop(state);
        if changed {
            self.publish_response_observation_binding();
        }
        route
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
        if observation.commentary_delivery_route != CommentaryDeliveryRoute::Wait {
            observation.commentary_delivery_route = CommentaryDeliveryRoute::Undecided;
        }
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

    pub(crate) fn response_observation_queue_delivery(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.turns.get(turn_id))
            .is_some_and(|observation| observation.queue_delivery)
    }

    pub(crate) fn response_observation_delivery_committed(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        response_item_id: &ResponseItemId,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.turns.get(turn_id))
            .is_some_and(|observation| {
                observation
                    .committed_delivery_response_item_ids
                    .contains(response_item_id)
            })
    }

    pub(crate) fn commit_response_observation_delivery(
        &self,
        commit: &ResponseObservationDeliveryCommit,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let terminal_text = state
            .response_terminals
            .get(&(commit.parent, commit.child, commit.turn_id.clone()))
            .and_then(|terminal| match &terminal.status {
                AgentStatus::Completed(text) => Some(text.clone().unwrap_or_default()),
                _ => None,
            });
        let Some(observation) = state
            .response_observation_by_observer_child
            .get_mut(&(commit.parent, commit.child))
            .and_then(|relationship| relationship.turns.get_mut(&commit.turn_id))
        else {
            return;
        };
        if observation
            .committed_delivery_response_item_ids
            .contains(&commit.response_item_id)
        {
            return;
        }
        let receipt_visibility = commit.model_visibility;
        let receipt_text = match commit.kind {
            ResponseObservationDeliveryKind::Commentary => observation
                .commentary_delivery
                .as_ref()
                .map(|delivery| delivery.text.clone()),
            ResponseObservationDeliveryKind::Final => terminal_text,
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
                observation.commentary_delivery_route = CommentaryDeliveryRoute::Mailbox;
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
        drop(state);
        if let Some(text) = receipt_text
            && self.bound_session_id().is_some_and(|root| {
                ThreadId::from(root) == commit.child.thread_id
                    && commit.parent.thread_id != commit.child.thread_id
            })
        {
            let control = self.clone();
            let commit = commit.clone();
            tokio::spawn(async move {
                let phase = match commit.kind {
                    ResponseObservationDeliveryKind::Commentary => {
                        codex_protocol::models::MessagePhase::Commentary
                    }
                    ResponseObservationDeliveryKind::Final => {
                        codex_protocol::models::MessagePhase::FinalAnswer
                    }
                };
                if let Err(error) = control
                    .mirror_agent_delivery_receipt(
                        commit.child.thread_id,
                        commit.parent.thread_id,
                        phase,
                        receipt_visibility,
                        commit.response_item_id.as_str(),
                        &text,
                    )
                    .await
                {
                    tracing::warn!(%error, "failed to present acknowledged response delivery");
                }
            });
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
                if pending.task_preview.is_some() {
                    current.task_preview = pending.task_preview.clone();
                    current.promoted_task_context = pending.promoted_task_context.clone();
                }
                current
                    .commentary_admissions
                    .extend(pending.commentary_admissions.iter().cloned());
                if current.commentary_delivery.is_none() {
                    current.commentary_delivery = pending.commentary_delivery.clone();
                }
                current.target_messages |= pending.target_messages;
                current.queue_delivery |= pending.queue_delivery;
                if current.message_wake_turn_id.is_none() {
                    current.message_wake_turn_id = pending.message_wake_turn_id.clone();
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
            let mut commentary = Vec::new();
            let mut pending = false;
            {
                let mut state = self.wait_agent_presentations.state();
                for target in target_turns {
                    let key = (parent, target.child, target.turn_id.clone());
                    if state.wait_commentary_turns.contains(&key) {
                        if let Some(observation) = state
                            .response_observation_by_observer_child
                            .get(&(parent, target.child))
                            .and_then(|relationship| relationship.turns.get(&target.turn_id))
                        {
                            if let Some(delivery) = observation.commentary_delivery.clone() {
                                commentary.push(WaitCommentaryDelivery {
                                    child: target.child,
                                    turn_id: target.turn_id.clone(),
                                    delivery,
                                });
                            } else if !observation.commentary_admissions.is_empty() {
                                pending = true;
                            }
                        }
                        continue;
                    }
                    let claimed = {
                        let Some(observation) = state
                            .response_observation_by_observer_child
                            .get_mut(&(parent, target.child))
                            .and_then(|relationship| relationship.turns.get_mut(&target.turn_id))
                        else {
                            continue;
                        };
                        if let Some(delivery) = observation.commentary_delivery.clone() {
                            if observation.commentary_delivery_route
                                == CommentaryDeliveryRoute::Undecided
                            {
                                observation.commentary_delivery_route =
                                    CommentaryDeliveryRoute::Wait;
                                Some(delivery)
                            } else {
                                None
                            }
                        } else {
                            if !observation.commentary_admissions.is_empty() {
                                pending = true;
                            }
                            None
                        }
                    };
                    if let Some(delivery) = claimed {
                        state.wait_commentary_turns.insert(key);
                        commentary.push(WaitCommentaryDelivery {
                            child: target.child,
                            turn_id: target.turn_id.clone(),
                            delivery,
                        });
                    }
                }
            }
            if !pending {
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
        self.publish_response_observation_binding();
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
