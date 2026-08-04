use super::*;
use crate::agent::control::AgentTerminalPresentation;
use crate::agent::control::CompletionParentAdoption;
use crate::agent::control::CompletionParentBinding;
use crate::agent::control::LocalAgentControl;
use crate::agent::control::TerminalPresentationDelivery;
use crate::codex_thread::CodexThread;

impl Session {
    pub(super) fn publish_agent_status_from_event(&self, envelope: &Event) {
        let event = &envelope.msg;
        let status = agent_status_from_event(event);
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .thread_removal_started
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let response_turn_id = match event {
            EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
            EventMsg::TurnComplete(event) => Some(event.turn_id.clone()),
            EventMsg::TurnAborted(event) => event.turn_id.clone(),
            EventMsg::Error(_) if status.as_ref().is_some_and(is_final) => {
                let state = self
                    .response_observation_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.live_turn_id.as_deref() == Some(envelope.id.as_str())
                    || state.terminal_outcomes.contains_key(&envelope.id))
                .then(|| envelope.id.clone())
            }
            EventMsg::ShutdownComplete if status.as_ref().is_some_and(is_final) => self
                .response_observation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .live_turn_id
                .clone(),
            _ => None,
        };
        let event_turn_id = response_turn_id.as_deref();
        if let Some(turn_id) = event_turn_id
            && let Some(status) = status.as_ref().filter(|status| is_final(status))
        {
            self.record_agent_response_terminal_observers(turn_id, status.clone());
            if matches!(event, EventMsg::Error(_) | EventMsg::ShutdownComplete) {
                self.publish_agent_response_terminal(turn_id.to_string(), status.clone());
            }
        }
        self.publish_agent_response_event(event);
        let Some(status) = status else { return };
        if let EventMsg::TurnStarted(event) = event {
            self.completion_parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .live_turn_id = Some(event.turn_id.clone());
        }
        let current_status = self.agent_status.borrow().clone();
        if event_turn_id.is_none_or(|turn_id| self.response_turn_can_publish_agent_status(turn_id))
            && (matches!(&status, AgentStatus::Running) || !is_final(&current_status))
        {
            self.replace_agent_status_locked(status);
        }
        if matches!(
            event,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::ShutdownComplete
        ) || agent_status_from_event(event)
            .as_ref()
            .is_some_and(is_final)
        {
            let mut parent = self
                .completion_parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if event_turn_id.is_none_or(|turn_id| parent.live_turn_id.as_deref() == Some(turn_id)) {
                parent.live_turn_id = None;
            }
        }
    }

    pub(crate) fn adopt_v1_completion_parent(
        &self,
        owner: LocalAgentControl,
        parent: &Arc<CodexThread>,
    ) -> CodexResult<(
        CompletionParentAdoption,
        Option<crate::agent::control::CompletionWatcherRegistration>,
    )> {
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_history_publication()?;
        parent.session.check_history_publication()?;
        parent.session.submission_admission.check_ready()?;
        let _adoption_admission = parent
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("completion parent is closing".to_string()))?;
        let mut state = self
            .completion_parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(binding) = &state.binding {
            let original = binding.parent.upgrade().ok_or_else(|| {
                CodexErr::InvalidRequest("original completion parent is no longer live".to_string())
            })?;
            original.session.check_history_publication()?;
            original.session.submission_admission.check_ready()?;
            let _original_admission = original
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("original completion parent is closing".to_string())
                })?;
            return Ok((
                if Arc::ptr_eq(&original, parent) {
                    CompletionParentAdoption::AlreadyBoundToCaller
                } else {
                    CompletionParentAdoption::OriginalParentPreserved
                },
                None,
            ));
        }
        if self.spawn_parent_thread_id.is_some() {
            return Err(CodexErr::InvalidRequest(
                "native child completion ownership must be restored through its original parent"
                    .to_string(),
            ));
        }
        let registration = owner
            .register_completion_watcher_with_parent(
                self.presentation_id(),
                parent,
                &self.thread_id.to_string(),
            )
            .ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "completion ownership already exists outside the adoption binding".to_string(),
                )
            })?;
        state.binding = Some(CompletionParentBinding {
            owner,
            parent: Arc::downgrade(parent),
        });
        Ok((CompletionParentAdoption::Adopted, Some(registration)))
    }

    /// Called under terminal_publication_lock. A cold status is never a live turn token.
    pub(super) fn capture_adopted_terminal_locked(&self, status: &AgentStatus) {
        let response_turn_id = self
            .completion_parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live_turn_id
            .clone();
        if let Some(turn_id) = response_turn_id {
            self.record_agent_response_terminal_observers(&turn_id, status.clone());
        }
        if self.spawn_parent_thread_id.is_some()
            || !self
                .terminal_presentation_armed
                .load(std::sync::atomic::Ordering::Acquire)
            || is_final(&self.agent_status.borrow())
        {
            return;
        }
        let mut state = self
            .completion_parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(turn_id) = state.live_turn_id.as_deref() else {
            return;
        };
        let Some(binding) = &state.binding else {
            return;
        };
        let Some(parent) = binding.parent.upgrade() else {
            return;
        };
        let publish_status = self.response_turn_can_publish_agent_status(turn_id);
        let _ = binding.owner.record_agent_terminal_presentation(
            parent.session.presentation_id(),
            self.presentation_id(),
            turn_id,
            status.clone(),
            TerminalPresentationDelivery::Watcher,
            || {
                if publish_status {
                    self.replace_agent_status_locked(status.clone());
                }
            },
        );
        state.live_turn_id = None;
    }

    fn record_sub_agent_terminal_presentation(
        &self,
        parent_thread_id: codex_protocol::ThreadId,
        turn_id: &str,
        status: AgentStatus,
        delivery: TerminalPresentationDelivery,
    ) -> Option<AgentTerminalPresentation> {
        if !self
            .terminal_presentation_armed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let _terminal_guard = self
            .terminal_publication_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_status = self.agent_status.borrow().clone();
        self.record_agent_response_terminal_observers(turn_id, status.clone());
        if is_final(&current_status) {
            return None;
        }
        let child = self.presentation_id();
        let parent = self
            .services
            .agent_control
            .completion_parent_for_child(child, parent_thread_id)?;
        let published_status = status.clone();
        let publish_status = self.response_turn_can_publish_agent_status(turn_id);
        self.services
            .agent_control
            .record_agent_terminal_presentation(parent, child, turn_id, status, delivery, || {
                if publish_status {
                    self.replace_agent_status_locked(published_status);
                }
            })
    }

    pub(super) async fn prepare_sub_agent_terminal_presentation(
        &self,
        turn_context: &TurnContext,
        event: &EventMsg,
    ) -> Option<AgentTerminalPresentation> {
        if self.spawn_parent_thread_id.is_none() {
            if let Some(status) = agent_status_from_event(event).filter(is_final) {
                let _terminal_guard = self
                    .terminal_publication_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Only a real TurnStarted in this runtime arms adoption. The current
                // TurnContext confirms its identity, rather than inventing a turn on resume.
                let matches_turn = self
                    .completion_parent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .live_turn_id
                    .as_deref()
                    == Some(turn_context.sub_id.as_str());
                if matches_turn {
                    self.capture_adopted_terminal_locked(&status);
                }
            }
            return None;
        }
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path,
            ..
        }) = &turn_context.session_source
        else {
            return None;
        };
        let delivery = match (turn_context.multi_agent_version, event) {
            (
                codex_protocol::protocol::MultiAgentVersion::V2,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_),
            ) if agent_path.is_some() => TerminalPresentationDelivery::Direct,
            _ => TerminalPresentationDelivery::Watcher,
        };
        let status = if delivery == TerminalPresentationDelivery::Direct {
            turn_context
                .terminal_error
                .lock()
                .await
                .as_ref()
                .map(|error| AgentStatus::Errored(error.message.clone()))
                .or_else(|| agent_status_from_event(event))?
        } else {
            agent_status_from_event(event)?
        };
        if !is_final(&status) {
            return None;
        }
        self.record_sub_agent_terminal_presentation(
            *parent_thread_id,
            &turn_context.sub_id,
            status,
            delivery,
        )
    }

    pub(super) fn prepare_raw_sub_agent_terminal_presentation(&self, event: &Event) {
        let Some(status) = agent_status_from_event(&event.msg).filter(is_final) else {
            return;
        };
        let Some(parent_thread_id) = self.spawn_parent_thread_id else {
            let _terminal_guard = self
                .terminal_publication_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let matches_turn = self
                .completion_parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .live_turn_id
                .as_ref()
                .is_some_and(|turn_id| {
                    *turn_id == event.id || matches!(&event.msg, EventMsg::ShutdownComplete)
                });
            if matches_turn {
                self.capture_adopted_terminal_locked(&status);
            }
            return;
        };
        let turn_id = match &event.msg {
            EventMsg::TurnComplete(event) => Some(event.turn_id.clone()),
            EventMsg::TurnAborted(event) => event.turn_id.clone(),
            EventMsg::Error(_) => {
                let state = self
                    .response_observation_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.live_turn_id.as_deref() == Some(event.id.as_str())
                    || state.terminal_outcomes.contains_key(&event.id))
                .then(|| event.id.clone())
            }
            _ => self
                .completion_parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .live_turn_id
                .clone(),
        };
        let Some(turn_id) = turn_id else { return };
        let _ = self.record_sub_agent_terminal_presentation(
            parent_thread_id,
            &turn_id,
            status,
            TerminalPresentationDelivery::Watcher,
        );
    }
}
