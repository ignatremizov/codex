//! Live exact-instance observations; historical snapshots never activate subscriptions.

use super::presentation::ResponseObservationEventMatch;
use super::presentation::ResponseWatcherRegistration;
use super::*;
use crate::codex_thread::CodexThread;
use crate::session::AgentResponseEvent;
use crate::session::AgentResponseSubscription;
use futures::future::BoxFuture;

pub(super) enum ResponseObserverStart {
    FutureOnly,
    CurrentOrNext(AgentStatus),
}

impl LocalAgentControl {
    pub(crate) async fn subscribe_terminal_status_events(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<(
        crate::session::TerminalStatusEvent,
        crate::session::TerminalStatusSubscription,
    )> {
        let thread = self.upgrade()?.get_thread(thread_id).await?;
        Ok(thread.session.subscribe_terminal_status_events())
    }

    pub(crate) async fn ensure_v1_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        source: SessionSource,
        policy: ResponseObservationPolicy,
        observed_status: AgentStatus,
    ) -> CodexResult<AgentStatus> {
        let control = self.clone();
        tokio::spawn(async move {
            control
                .install_live_response_observer(child_thread_id, source, policy, observed_status)
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("response adoption worker failed: {error}")))?
    }

    async fn install_live_response_observer(
        &self,
        child_thread_id: ThreadId,
        source: SessionSource,
        policy: ResponseObservationPolicy,
        observed_status: AgentStatus,
    ) -> CodexResult<AgentStatus> {
        // Additional observers cannot rewrite the native parent or its registry validation.
        self.ensure_native_v1_completion_watcher(child_thread_id, source.clone())
            .await?;
        let parent_id = source.parent_thread_id().ok_or_else(|| {
            CodexErr::InvalidRequest("response observation requires an observer".to_string())
        })?;
        let state = self.upgrade()?;
        let _lifecycle = state.acquire_live_agent_lifecycle(child_thread_id).await?;
        let child = state.get_thread(child_thread_id).await?;
        let observer = state.get_thread(parent_id).await?;
        self.ensure_target_message_route_allowed(
            &child,
            observer.session.presentation_id(),
            policy,
        )?;
        let _transaction = self
            .acquire_response_observation_transaction(observer.session.presentation_id())
            .await;
        self.install_response_observer(
            &observer,
            &child,
            policy,
            ResponseObservationBinding::NextTurn,
            ResponseObserverStart::CurrentOrNext(observed_status),
        )
        .await?;
        Ok(child.agent_status().await)
    }

    pub(super) fn install_response_observer<'a>(
        &'a self,
        observer: &'a Arc<CodexThread>,
        target: &'a Arc<CodexThread>,
        policy: ResponseObservationPolicy,
        binding: ResponseObservationBinding,
        start: ResponseObserverStart,
    ) -> BoxFuture<'a, CodexResult<()>> {
        Box::pin(async move {
            self.ensure_scoped_reply_route_supported(target, policy)?;
            let parent = observer.session.presentation_id();
            let child = target.session.presentation_id();
            if parent == child {
                return Err(CodexErr::InvalidRequest(
                    "an agent cannot observe itself".to_string(),
                ));
            }
            if !Arc::ptr_eq(&self.state, &observer.session.services.agent_control.state) {
                return Err(CodexErr::InvalidRequest(
                    "observer belongs to another control".to_string(),
                ));
            }
            observer.session.submission_admission.check_ready()?;
            target.session.submission_admission.check_ready()?;
            let _observer_admission = observer
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".to_string()))?;
            let state = self.upgrade()?;
            let generation = state.agent_lifecycle_generation(child.thread_id);
            let started_control = self.clone();
            let terminal_control = self.clone();
            let (_, responses, (registration, reconciled_terminal)) =
                target.session.subscribe_agent_responses_observing_turns(
                    move |turn_id, sequence| {
                        if let Ok(state) = started_control.upgrade() {
                            let _ = state.with_current_agent_lifecycle_generation(
                                child.thread_id,
                                generation,
                                || {
                                    started_control
                                        .bind_response_observation_started_turn_at_sequence(
                                            parent, child, turn_id, sequence,
                                        );
                                },
                            );
                        }
                    },
                    move |turn_id, status| {
                        if let Ok(state) = terminal_control.upgrade() {
                            let _ = state.with_current_agent_lifecycle_generation(
                                child.thread_id,
                                generation,
                                || {
                                    terminal_control.record_response_observation_terminal(
                                        parent, child, turn_id, status,
                                    );
                                },
                            );
                        }
                    },
                    |snapshot| {
                        let reconciled_terminal = match &start {
                            ResponseObserverStart::CurrentOrNext(previous)
                                if !crate::agent::status::is_final(previous)
                                    && snapshot.active_turn_id.is_none()
                                    && crate::agent::status::is_final(&snapshot.status) =>
                            {
                                snapshot.last_terminal.clone()
                            }
                            ResponseObserverStart::FutureOnly
                            | ResponseObserverStart::CurrentOrNext(_) => None,
                        };
                        let target_turn = match &start {
                            ResponseObserverStart::FutureOnly => None,
                            ResponseObserverStart::CurrentOrNext(_) => {
                                snapshot.active_turn_id.clone().or_else(|| {
                                    reconciled_terminal.as_ref().map(|(turn, _)| turn.clone())
                                })
                            }
                        };
                        let registration = self.register_response_watcher_with_parent_at_sequence(
                            child,
                            observer,
                            policy,
                            /*retain_passive_completion_relationship*/ false,
                            target_turn,
                            binding,
                            ResponseObservationPersistence::Durable,
                            snapshot.next_event_sequence,
                            snapshot.last_commentary_item_id.clone(),
                        );
                        (registration, reconciled_terminal)
                    },
                );
            if binding == ResponseObservationBinding::NextTurn
                && let Err(error) = self
                    .persist_response_observation_snapshot(parent, child)
                    .await
            {
                self.abandon_response_observer(parent, child, &error.to_string());
                return Err(error);
            }
            if let Some((turn_id, status)) = reconciled_terminal {
                self.record_response_observation_terminal(parent, child, &turn_id, status);
            }
            if let Some(registration) = registration {
                let control = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = control
                        .run_response_observer(parent, child, generation, registration, responses)
                        .await
                    {
                        control.abandon_response_observer(parent, child, &error.to_string());
                        warn!("response observer stopped: {error}");
                    }
                });
            }
            Ok(())
        })
    }

    async fn run_response_observer(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        generation: u64,
        mut registration: ResponseWatcherRegistration,
        mut responses: AgentResponseSubscription,
    ) -> CodexResult<()> {
        loop {
            let terminal_changed = self
                .wait_agent_presentations
                .watcher_terminal_changed
                .notified();
            tokio::pin!(terminal_changed);
            terminal_changed.as_mut().enable();
            // Preserve event order: complete commentary already queued before a terminal
            // must consume its own admitted cursor before final delivery retires the turn.
            let buffered_response = responses.try_recv();
            // Accepted terminals retain their exact observer even if close revokes future work.
            if buffered_response.is_none() {
                self.drain_observed_terminals(parent, child).await?;
            }
            let state = self.upgrade()?;
            let changed = state.wait_for_agent_lifecycle_change();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !state.agent_lifecycle_generation_is_current(child.thread_id, generation) {
                self.revoke_response_observations_for_child(child);
                self.drain_observed_terminals(parent, child).await?;
                self.recheck_thread_idle_lifecycle(parent).await;
                return Ok(());
            }
            if registration.retire_if_observation_idle() {
                self.recheck_thread_idle_lifecycle(parent).await;
                return Ok(());
            }
            let observer_alive = state
                .get_thread(parent.thread_id)
                .await
                .is_ok_and(|observer| {
                    observer.session.presentation_id() == parent
                        && observer.session.submission_admission.check_ready().is_ok()
                });
            if !observer_alive {
                self.revoke_response_observation_for_presentation(parent, child);
                self.drain_observed_terminals(parent, child).await?;
                return Ok(());
            }
            drop(state);
            let response = match buffered_response {
                Some(response) => Some(response),
                None => tokio::select! {
                    biased;
                    () = &mut changed => continue,
                    () = &mut terminal_changed => continue,
                    response = responses.recv() => response,
                },
            };
            let Some(response) = response else {
                if self.has_future_response_observation(parent, child) {
                    registration.preserve_state_for_replacement_on_drop();
                    self.reconnect_future_response_observer(parent, child, generation)
                        .await?;
                }
                return Ok(());
            };
            match response {
                AgentResponseEvent::TurnStarted { turn_id, sequence } => {
                    let _transaction = self.acquire_response_observation_transaction(parent).await;
                    self.bind_response_observation_started_turn_at_sequence(
                        parent, child, &turn_id, sequence,
                    );
                    self.persist_response_observation_snapshot(parent, child)
                        .await?;
                }
                AgentResponseEvent::Commentary {
                    turn_id,
                    item_id,
                    text,
                    sequence,
                } => {
                    if self
                        .await_response_observation_event_match(parent, child, &turn_id)
                        .await
                    {
                        self.deliver_observed_commentary(
                            parent, child, generation, &turn_id, &item_id, &text, sequence,
                        )
                        .await?;
                    }
                }
                AgentResponseEvent::Terminal { turn_id, status } => {
                    if self
                        .await_response_observation_event_match(parent, child, &turn_id)
                        .await
                    {
                        self.record_response_observation_terminal(parent, child, &turn_id, status);
                    }
                }
                AgentResponseEvent::TurnAborted { turn_id } => {
                    if self
                        .await_response_observation_event_match(parent, child, &turn_id)
                        .await
                    {
                        self.record_response_observation_terminal(
                            parent,
                            child,
                            &turn_id,
                            AgentStatus::Interrupted,
                        );
                    }
                }
            }
        }
    }

    async fn drain_observed_terminals(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> CodexResult<()> {
        while let Some(terminal) = self.take_response_observation_terminal(parent, child) {
            if self
                .await_response_observation_event_match(parent, child, &terminal.turn_id)
                .await
            {
                self.deliver_observed_terminal(child, terminal).await?;
            }
        }
        Ok(())
    }

    async fn reconnect_future_response_observer(
        &self,
        parent: SessionPresentationId,
        old_child: SessionPresentationId,
        generation: u64,
    ) -> CodexResult<()> {
        let mut created = self.upgrade()?.subscribe_thread_created();
        loop {
            let state = self.upgrade()?;
            let changed = state.wait_for_agent_lifecycle_change();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !state.agent_lifecycle_generation_is_current(old_child.thread_id, generation) {
                return Ok(());
            }
            let observer = state.get_thread(parent.thread_id).await?;
            if observer.session.presentation_id() != parent {
                return Ok(());
            }
            if let Ok(target) = state.get_thread(old_child.thread_id).await
                && target.session.presentation_id() != old_child
            {
                let _lifecycle = state
                    .v2_spawn_resume_lock(old_child.thread_id)
                    .lock_owned()
                    .await;
                let current = state.get_thread(old_child.thread_id).await?;
                if !Arc::ptr_eq(&target, &current) {
                    continue;
                }
                if !state.agent_lifecycle_generation_is_current(old_child.thread_id, generation) {
                    return Ok(());
                }
                let _transaction = self.acquire_response_observation_transaction(parent).await;
                if !self.move_future_response_observation(
                    parent,
                    old_child,
                    current.session.presentation_id(),
                ) {
                    return Ok(());
                }
                return self
                    .install_response_observer(
                        &observer,
                        &current,
                        ResponseObservationPolicy::from_parts(
                            /*commentary*/ false,
                            FinalResponseObservation::None,
                        ),
                        ResponseObservationBinding::NextTurn,
                        ResponseObserverStart::FutureOnly,
                    )
                    .await;
            }
            drop(observer);
            drop(state);
            tokio::select! {
                () = &mut changed => {},
                result = created.recv() => {
                    if matches!(result, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return Ok(());
                    }
                }
            }
        }
    }

    pub(crate) async fn recheck_thread_idle_lifecycle(&self, observer: SessionPresentationId) {
        if let Ok(state) = self.upgrade()
            && let Ok(thread) = state.get_thread(observer.thread_id).await
            && thread.session.presentation_id() == observer
        {
            thread
                .session
                .emit_thread_idle_lifecycle_if_idle(codex_extension_api::ThreadIdleCause::Completed)
                .await;
        }
    }

    pub(super) async fn await_response_observation_event_match(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> bool {
        loop {
            let notified = self
                .wait_agent_presentations
                .response_observation_changed
                .notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.response_observation_event_match(parent, child, turn_id) {
                ResponseObservationEventMatch::Observe => return true,
                ResponseObservationEventMatch::Ignore => return false,
                ResponseObservationEventMatch::AwaitBinding => notified.await,
            }
        }
    }
}
