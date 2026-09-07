use super::*;

impl LocalAgentControl {
    /// Fail closed after a lost observation receipt. Ordinary close must instead retain
    /// accepted obligations; this path is only for an observer whose history is quarantined.
    pub(in crate::agent::control) fn abandon_response_observer(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        reason: &str,
    ) {
        let mut state = self.wait_agent_presentations.state();
        let observer = state
            .response_observers
            .get(&(parent, child))
            .and_then(Weak::upgrade);
        state.revoke_response_observation((parent, child));
        state.response_queued.remove(&(parent, child));
        let keys = state
            .response_terminals
            .keys()
            .filter(|(observer, target, _)| *observer == parent && *target == child)
            .cloned()
            .collect::<Vec<_>>();
        let terminals = keys
            .into_iter()
            .filter_map(|key| {
                let terminal = state.response_terminals.remove(&key)?;
                state.contexts.remove(&terminal.context_id);
                Some(AgentTerminalPresentation { inner: terminal })
            })
            .collect::<Vec<_>>();
        if let Some(relationship) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, child))
        {
            for turn in relationship.turns.values_mut() {
                turn.final_response = FinalResponseObservation::None;
                turn.commentary_delivery = None;
            }
        }
        drop(state);
        if let Some(observer) = observer {
            observer.session.quarantine_history(reason.to_owned());
        }
        for terminal in terminals {
            if let Some(observer) = terminal.take_parent_thread() {
                observer.session.quarantine_history(reason.to_owned());
            }
            drop(terminal.take_accepted_completion_delivery());
        }
        self.publish_response_observation_binding();
        self.wait_agent_presentations
            .watcher_terminal_changed
            .notify_waiters();
    }

    pub(crate) async fn acquire_response_observation_transaction(
        &self,
        observer: SessionPresentationId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let transaction = self
            .wait_agent_presentations
            .observation_transactions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(observer)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        transaction.lock_owned().await
    }

    /// Records the same immutable terminal used by native completion, without establishing
    /// native parentage. Only an exact, live observer can reserve a new delivery.
    pub(crate) fn record_response_observation_terminal(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
        status: AgentStatus,
    ) -> Option<AgentTerminalPresentation> {
        let mut state = self.wait_agent_presentations.state();
        let key = (parent, child, turn_id.to_owned());
        if let Some(inner) = state.response_terminals.get(&key) {
            let status = inner.status.clone();
            let presentation = AgentTerminalPresentation {
                inner: Arc::clone(inner),
            };
            let queued = state.response_queued.entry((parent, child)).or_default();
            if !queued.iter().any(|terminal| terminal.turn_id == turn_id) {
                queued.push_back(WatcherTerminalPresentation {
                    turn_id: turn_id.to_owned(),
                    status,
                    presentation: presentation.clone(),
                });
            }
            drop(state);
            self.wait_agent_presentations
                .watcher_terminal_changed
                .notify_waiters();
            return Some(presentation);
        }
        let observation = state
            .response_observation_by_observer_child
            .get(&(parent, child))?
            .turns
            .get(turn_id)?;
        if observation.final_response == FinalResponseObservation::None {
            return None;
        }
        let parent_thread = state.response_observers.get(&(parent, child))?.upgrade()?;
        if parent_thread.session.presentation_id() != parent {
            return None;
        }
        let accepted = parent_thread
            .session
            .submission_admission
            .try_accept_completion_delivery()?;
        let child_reference = state
            .parents
            .get(&child)
            .filter(|binding| {
                binding
                    .thread
                    .upgrade()
                    .is_some_and(|thread| thread.session.presentation_id() == parent)
            })
            .map(|binding| binding.child_reference.clone())
            .unwrap_or_else(|| child.thread_id.to_string());
        let item = codex_protocol::protocol::sub_agent_completion_item(&child_reference, &status)?;
        let waits = state
            .waits
            .iter()
            .filter_map(|(id, wait)| {
                (wait.parent == parent
                    && wait
                        .children
                        .as_ref()
                        .is_none_or(|children| children.contains(&child.thread_id)))
                .then_some(*id)
            })
            .collect::<HashSet<_>>();
        let sequence = state.next_terminal;
        state.next_terminal = state.next_terminal.saturating_add(1);
        let inner = Arc::new(Terminal {
            parent,
            child,
            turn_id: turn_id.to_owned(),
            sequence,
            parent_thread: Mutex::new(Some(parent_thread)),
            context_id: new_sub_agent_completion_context_response_item_id(),
            status: status.clone(),
            presentation: CompletionPresentation {
                item: TurnItem::AgentMessage(item),
                history_only_turn_id: Uuid::now_v7().to_string(),
            },
            observation_presentation: OnceLock::new(),
            accepted: Mutex::new(Some(accepted)),
            ownership: Mutex::new(Ownership {
                waits: waits.clone(),
                presenter: None,
                background_claimed: false,
                committed: false,
            }),
            changed: Notify::new(),
        });
        for wait_id in waits {
            if let Some(wait) = state.waits.get_mut(&wait_id) {
                wait.terminals.push(Arc::downgrade(&inner));
            }
        }
        state
            .contexts
            .insert(inner.context_id.clone(), Arc::clone(&inner));
        state.response_terminals.insert(key, Arc::clone(&inner));
        let presentation = AgentTerminalPresentation { inner };
        state
            .response_queued
            .entry((parent, child))
            .or_default()
            .push_back(WatcherTerminalPresentation {
                turn_id: turn_id.to_owned(),
                status,
                presentation: presentation.clone(),
            });
        drop(state);
        self.wait_agent_presentations
            .watcher_terminal_changed
            .notify_waiters();
        Some(presentation)
    }

    pub(crate) fn take_response_observation_terminal(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Option<WatcherTerminalPresentation> {
        self.wait_agent_presentations
            .state()
            .response_queued
            .get_mut(&(parent, child))?
            .pop_front()
    }

    pub(crate) fn response_observation_terminal(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> Option<AgentTerminalPresentation> {
        self.wait_agent_presentations
            .state()
            .response_terminals
            .get(&(parent, child, turn_id.to_owned()))
            .cloned()
            .map(|inner| AgentTerminalPresentation { inner })
    }

    pub(crate) fn has_future_response_observation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> bool {
        self.wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .get(&(parent, child))
            .is_some_and(|relationship| {
                relationship.baseline_final_response != FinalResponseObservation::None
                    || relationship.pending_next_turn.is_some()
            })
    }

    /// Moves future policy after the owner validates the replacement target and close generation.
    /// Historical turn claims and accepted delivery capabilities stay with the original instance.
    pub(crate) fn move_future_response_observation(
        &self,
        parent: SessionPresentationId,
        old_child: SessionPresentationId,
        new_child: SessionPresentationId,
    ) -> bool {
        if old_child == new_child
            || old_child.thread_id != new_child.thread_id
            || new_child.instance_id.is_nil()
        {
            return false;
        }
        let mut state = self.wait_agent_presentations.state();
        let Some(old) = state
            .response_observation_by_observer_child
            .get_mut(&(parent, old_child))
        else {
            return false;
        };
        let baseline = old.baseline_final_response;
        let pending = old.pending_next_turn.take();
        let persistence = old.persistence;
        old.baseline_final_response = FinalResponseObservation::None;
        if baseline == FinalResponseObservation::None && pending.is_none() {
            return false;
        }
        let replacement = state
            .response_observation_by_observer_child
            .entry((parent, new_child))
            .or_default();
        replacement.persistence = replacement.persistence.max(persistence);
        replacement.baseline_final_response = replacement.baseline_final_response.max(baseline);
        if let Some(mut pending) = pending {
            // Event cursors belong to a runtime instance. The replacement's actual turn-start
            // binds a fresh boundary; old sequence numbers cannot gate its future commentary.
            for admission in &mut pending.commentary_admissions {
                admission.minimum_event_sequence = 0;
                admission.after_item_id = None;
            }
            let current = replacement
                .pending_next_turn
                .get_or_insert_with(Default::default);
            current.final_response = current.final_response.max(pending.final_response);
            current.target_messages |= pending.target_messages;
            current.queue_delivery |= pending.queue_delivery;
            current
                .commentary_admissions
                .extend(pending.commentary_admissions);
        }
        drop(state);
        self.publish_response_observation_binding();
        true
    }

    pub(crate) fn revoke_response_observations_for_child(&self, child: SessionPresentationId) {
        let mut state = self.wait_agent_presentations.state();
        let pairs = state
            .response_observation_by_observer_child
            .keys()
            .filter(|(_, target)| *target == child)
            .copied()
            .collect::<Vec<_>>();
        for pair in pairs {
            state.revoke_response_observation(pair);
        }
        drop(state);
        self.publish_response_observation_binding();
    }

    pub(crate) fn revoke_response_observation_for_presentation(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) {
        let mut state = self.wait_agent_presentations.state();
        state.revoke_response_observation((parent, child));
        drop(state);
        self.publish_response_observation_binding();
    }
}

impl PresentationState {
    pub(in crate::agent::control::presentation) fn revoke_response_observation(
        &mut self,
        pair: (SessionPresentationId, SessionPresentationId),
    ) {
        self.response_observers.remove(&pair);
        let Some(relationship) = self.response_observation_by_observer_child.get_mut(&pair) else {
            return;
        };
        relationship.revoked = true;
        relationship.reply_route = None;
        relationship.baseline_final_response = FinalResponseObservation::None;
        relationship.pending_next_turn = None;
        relationship.pending_admissions.clear();
        for (turn_id, observation) in &mut relationship.turns {
            observation.commentary_admissions.clear();
            observation.message_wake_reservation_id = None;
            observation.target_messages = false;
            let accepted = self
                .response_terminals
                .contains_key(&(pair.0, pair.1, turn_id.clone()))
                || observation
                    .final_delivery_response_item_id
                    .as_ref()
                    .is_some_and(|id| {
                        !observation
                            .committed_delivery_response_item_ids
                            .contains(id)
                    });
            if !accepted {
                observation.final_response = FinalResponseObservation::None;
            }
        }
    }
}
