use super::presentation::FinalResponseObservationReplacement;
use super::presentation::ReplacedFinalResponseObservationBinding;
use super::*;
use crate::session::AgentResponseSubscription;
use crate::session::SteerInputError;
use crate::session::agent_response_events_from_rollout;
use codex_history::rollout::rollout_without_exact_rollback_ranges;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;
use std::collections::HashSet;

pub(crate) struct ReplacedFinalResponseObservation {
    pub(crate) target_thread_id: ThreadId,
    pub(crate) previous: FinalResponseObservation,
    pub(crate) binding: ReplacedFinalResponseObservationBinding,
}

struct ResponseObservationReplacementCommitGuard {
    parent_thread: Arc<CodexThread>,
    committed: bool,
}

impl ResponseObservationReplacementCommitGuard {
    fn new(parent_thread: Arc<CodexThread>) -> Self {
        Self {
            parent_thread,
            committed: false,
        }
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ResponseObservationReplacementCommitGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.parent_thread
                .session
                .submission_admission
                .rollback_requires_reload();
        }
    }
}

pub(super) struct CompletionWatcherLifecycleGuard {
    control: AgentControl,
    registration: Option<super::presentation::CompletionWatcherRegistration>,
    parent: SessionPresentationId,
    child: SessionPresentationId,
}

impl CompletionWatcherLifecycleGuard {
    pub(super) fn new(
        control: AgentControl,
        registration: super::presentation::CompletionWatcherRegistration,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) -> Self {
        Self {
            control,
            registration: Some(registration),
            parent,
            child,
        }
    }

    pub(super) fn take_registration(
        &mut self,
    ) -> Option<super::presentation::CompletionWatcherRegistration> {
        let registration = self.registration.take()?;
        registration.is_current().then_some(registration)
    }

    pub(super) fn retire_if_observation_idle(&mut self) -> bool {
        self.registration.as_mut().is_none_or(
            super::presentation::CompletionWatcherRegistration::retire_if_observation_idle,
        )
    }
}

impl Drop for CompletionWatcherLifecycleGuard {
    fn drop(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };
        let removed_bound_wake = self
            .control
            .has_bound_final_response_wake_for_target(self.parent, self.child);
        drop(registration);
        if removed_bound_wake && let Ok(runtime_handle) = tokio::runtime::Handle::try_current() {
            let control = self.control.clone();
            let parent = self.parent;
            runtime_handle.spawn(async move {
                control.recheck_thread_idle_lifecycle(parent).await;
            });
        }
    }
}

async fn validate_response_observation_endpoints(
    state: &Arc<ThreadManagerState>,
    parent: SessionPresentationId,
    child: SessionPresentationId,
    child_lifecycle_generation: u64,
) -> CodexResult<()> {
    let parent_thread = state.get_thread_including_pending(parent.thread_id).await?;
    if parent_thread.session.presentation_id() != parent {
        return Err(CodexErr::ThreadNotFound(parent.thread_id));
    }
    if !state.agent_lifecycle_generation_is_current(child.thread_id, child_lifecycle_generation) {
        return Err(CodexErr::ThreadNotFound(child.thread_id));
    }
    let child_thread = state.get_thread_including_pending(child.thread_id).await?;
    if child_thread.session.presentation_id() != child {
        return Err(CodexErr::ThreadNotFound(child.thread_id));
    }
    Ok(())
}

impl AgentControl {
    /// Revoke response observation owned before an exclusive subtree transfer.
    ///
    /// The caller invokes this immediately after the durable alias transaction commits, while it
    /// still holds every transferred lifecycle lock and before it installs the destination
    /// watcher or publishes the replacement runtime.
    pub(super) async fn invalidate_transferred_subtree_response_observers(
        &self,
        previous_session_id: Option<SessionId>,
        target_thread_id: ThreadId,
        descendant_thread_ids: &[ThreadId],
    ) {
        if previous_session_id == Some(self.session_id()) {
            return;
        }
        let Ok(state) = self.upgrade() else {
            return;
        };
        // Advance every generation before the first await. Cancellation immediately after the
        // durable transfer commit can then unload destination setup, but cannot leave any
        // former-owner recovery generation valid for a later resume.
        for thread_id in
            std::iter::once(target_thread_id).chain(descendant_thread_ids.iter().copied())
        {
            state.advance_agent_lifecycle_generation(thread_id);
        }
        state.agent_turn_queue.cancel_for_threads(
            std::iter::once(target_thread_id).chain(descendant_thread_ids.iter().copied()),
        );

        let Some(previous_session_id) = previous_session_id else {
            return;
        };
        let Ok(previous_root) = state
            .get_thread_including_pending(ThreadId::from(previous_session_id))
            .await
        else {
            // Generation invalidation remains manager-wide, so unloaded former controls and
            // independently owned observers still reject the transferred runtime on recovery.
            return;
        };
        let previous_control = previous_root.session.services.agent_control.clone();
        if previous_control.session_id() != previous_session_id {
            return;
        }
        let mut affected_wake_observers = HashSet::new();
        for thread_id in
            std::iter::once(target_thread_id).chain(descendant_thread_ids.iter().copied())
        {
            affected_wake_observers
                .extend(previous_control.revoke_response_observations_for_child(thread_id));
        }
        for observer in affected_wake_observers {
            // Transfer still holds every subtree lifecycle lock. Re-evaluate the former
            // observer after this synchronous revocation without making transfer setup depend on
            // any goal turn that the newly idle parent may start.
            let previous_control = previous_control.clone();
            tokio::spawn(async move {
                previous_control
                    .recheck_thread_idle_lifecycle(observer)
                    .await;
            });
        }
    }

    pub(crate) async fn replace_durable_final_response_observation(
        &self,
        target_thread_id: ThreadId,
        parent: SessionPresentationId,
        replacement: FinalResponseObservation,
    ) -> CodexResult<ReplacedFinalResponseObservation> {
        let state = self.upgrade()?;
        let lifecycle_lock = state.agent_lifecycle_lock(target_thread_id);
        let _lifecycle_guard = lifecycle_lock.lock_owned().await;
        self.require_current_agent_ownership(target_thread_id)
            .await?;
        let _submission_permit = self
            .acquire_mailbox_submission_permit(target_thread_id)
            .await?;
        let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
        let child_thread = state.get_thread_including_pending(target_thread_id).await?;
        let child = child_thread.session.presentation_id();
        let child_lifecycle_generation = state.agent_lifecycle_generation(target_thread_id);
        validate_response_observation_endpoints(&state, parent, child, child_lifecycle_generation)
            .await?;
        let (response_snapshot, response_rx) = child_thread.session.subscribe_agent_responses();
        drop(response_rx);
        let prepared = match self.prepare_final_response_observation_replacement(
            parent,
            child,
            response_snapshot.active_turn_id.as_deref(),
            response_snapshot
                .last_terminal
                .as_ref()
                .map(|(turn_id, _status)| turn_id.as_str()),
            replacement,
        ) {
            Ok(prepared) => prepared,
            Err(FinalResponseObservationReplacement::Replaced { .. }) => {
                return Err(CodexErr::Fatal(
                    "prepared response observation unexpectedly reported a committed replacement"
                        .to_string(),
                ));
            }
            Err(FinalResponseObservationReplacement::NoObservation) => {
                return Err(CodexErr::InvalidRequest(format!(
                    "agent {target_thread_id} has no active or pending response observation"
                )));
            }
            Err(FinalResponseObservationReplacement::DeliveryClaimed) => {
                return Err(CodexErr::InvalidRequest(format!(
                    "agent {target_thread_id} response delivery is already committed"
                )));
            }
        };
        let parent_thread = state.get_thread_including_pending(parent.thread_id).await?;
        if parent_thread.session.presentation_id() != parent {
            return Err(CodexErr::InvalidRequest(
                "agent response observer is no longer current".to_string(),
            ));
        }
        let task_item = if prepared.previous == FinalResponseObservation::PresentationOnly
            && matches!(
                replacement,
                FinalResponseObservation::Passive | FinalResponseObservation::Wake
            ) {
            match prepared.task_preview.clone() {
                Some(task_preview) => {
                    match self
                        .prepare_user_agent_task_context_item(
                            parent,
                            target_thread_id,
                            task_preview,
                        )
                        .await?
                    {
                        Some((task_parent_thread, task_item))
                            if Arc::ptr_eq(&task_parent_thread, &parent_thread) =>
                        {
                            Some(task_item)
                        }
                        Some(_) => {
                            return Err(CodexErr::InvalidRequest(
                                "agent response observer was replaced while preparing task context"
                                    .to_string(),
                            ));
                        }
                        None => None,
                    }
                }
                None => None,
            }
        } else {
            None
        };
        let mut observations =
            self.prepared_response_observation_replacement_snapshots(parent, child, &prepared);
        if observations.is_empty() {
            return Err(CodexErr::Fatal(
                "durable response observation replacement produced no persistence snapshot"
                    .to_string(),
            ));
        }
        if let Some(task_item) = task_item {
            let Some(replaced_observation) = observations
                .iter_mut()
                .find(|observation| observation.target_turn_id == prepared.target_turn_id)
            else {
                return Err(CodexErr::Fatal(
                    "durable response observation replacement omitted its changed turn snapshot"
                        .to_string(),
                ));
            };
            replaced_observation.promoted_task_context = Some(
                AgentResponsePromotedTaskContext::from_response_item(&task_item).ok_or_else(
                    || {
                        CodexErr::Fatal(
                            "prepared user-agent task context had an invalid durable shape"
                                .to_string(),
                        )
                    },
                )?,
            );
        }
        let commit_guard =
            ResponseObservationReplacementCommitGuard::new(Arc::clone(&parent_thread));
        if !parent_thread
            .session
            .persist_agent_response_observation_replacement(observations.as_slice())
            .await
        {
            return Err(CodexErr::Fatal(
                "failed to persist replaced response observation state".to_string(),
            ));
        }
        if !self.commit_final_response_observation_replacement(parent, child, &prepared) {
            return Err(CodexErr::Fatal(
                "response observation changed while its replacement was being persisted; refresh the thread before continuing"
                    .to_string(),
            ));
        }
        commit_guard.commit();
        drop(_transaction_permit);
        drop(_submission_permit);
        drop(_lifecycle_guard);
        if prepared.previous == FinalResponseObservation::Wake
            && replacement != FinalResponseObservation::Wake
        {
            self.recheck_thread_idle_lifecycle(parent).await;
        }
        Ok(ReplacedFinalResponseObservation {
            target_thread_id,
            previous: prepared.previous,
            binding: prepared.binding,
        })
    }

    fn revoke_previous_response_observation(
        &self,
        parent: SessionPresentationId,
        previous_child: Option<SessionPresentationId>,
    ) {
        if let Some(previous_child) = previous_child
            && self.revoke_response_observation_for_presentation(parent, previous_child)
        {
            // The lifecycle boundary may belong to another AgentControl. Re-check this
            // observer's local idle lifecycle after its generation listener revokes the old wake.
            let control = self.clone();
            tokio::spawn(async move {
                control.recheck_thread_idle_lifecycle(parent).await;
            });
        }
    }

    fn response_observer_generation_is_current(
        &self,
        parent: SessionPresentationId,
        target_thread_id: ThreadId,
        target_lifecycle_generation: u64,
        previous_child: Option<SessionPresentationId>,
    ) -> bool {
        if self.agent_lifecycle_generation_is_current(target_thread_id, target_lifecycle_generation)
        {
            return true;
        }
        // Explicit close and ownership transfer invalidate the old generation across every
        // AgentControl. Revoke only the presentation owned by this recovery task so a fresh
        // post-boundary resume using the same rollout thread UUID remains intact.
        self.revoke_previous_response_observation(parent, previous_child);
        false
    }

    pub(super) async fn response_observer_can_retry(&self, parent: SessionPresentationId) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread_including_pending(parent.thread_id).await else {
            return false;
        };
        parent_thread.session.presentation_id() == parent
            && parent_thread.io.session_loop_termination.peek().is_none()
            && parent_thread
                .session
                .submission_admission
                .response_observation_delivery_can_retry()
    }

    pub(super) async fn terminal_response_observer_can_retry(
        &self,
        parent: SessionPresentationId,
        presentation: &AgentTerminalPresentation,
    ) -> bool {
        if !presentation.has_accepted_completion_delivery() {
            return self.response_observer_can_retry(parent).await;
        }
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(parent_thread) = state.get_thread_including_pending(parent.thread_id).await else {
            return false;
        };
        parent_thread.session.presentation_id() == parent
            && parent_thread.io.session_loop_termination.peek().is_none()
            && parent_thread
                .session
                .submission_admission
                .response_observation_accepted_delivery_can_retry()
    }

    /// Ensures an explicitly adopted live v1 thread reports its final lifecycle status.
    ///
    /// A thread can already be live because another client resumed it through the same durable
    /// owner. Registering is idempotent for the child presentation and revalidates ownership under
    /// the target lifecycle boundary so a concurrent transfer cannot become cross-root
    /// observation.
    pub(crate) async fn ensure_v1_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
        observed_status: AgentStatus,
    ) -> CodexResult<AgentStatus> {
        self.ensure_durable_completion_watcher_inner(
            child_thread_id,
            session_source,
            response_observation,
            InitialTerminalObservation::ReconcileIfAdvancedFrom(observed_status),
            /*task_preview*/ None,
        )
        .await
    }

    /// Ensures an explicitly adopted live thread reports through durable response observation.
    ///
    /// User control deliberately uses this transport for both V1 and V2 targets so its response
    /// policy is exact-turn and native V2 completion publication cannot duplicate it. Unlike a
    /// model-authored bare-`x` resume of an already idle target, prompt-less user resume reserves
    /// every response mode for the next user-authored target turn.
    pub(crate) async fn ensure_durable_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
        observed_status: AgentStatus,
        task_preview: Option<String>,
    ) -> CodexResult<AgentStatus> {
        self.ensure_durable_completion_watcher_inner(
            child_thread_id,
            session_source,
            response_observation,
            InitialTerminalObservation::ReconcileOrObserveNextFrom(observed_status),
            task_preview,
        )
        .await
    }

    async fn ensure_durable_completion_watcher_inner(
        &self,
        child_thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
        initial_terminal_observation: InitialTerminalObservation,
        task_preview: Option<String>,
    ) -> CodexResult<AgentStatus> {
        let state = self.upgrade()?;
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &session_source
        else {
            return Ok(self.get_status(child_thread_id).await);
        };
        if *parent_thread_id == child_thread_id {
            return Ok(self.get_status(child_thread_id).await);
        }
        // Live adoption adds an observation edge; it does not mutate parent/child membership.
        // Snapshot the target under its own lifecycle boundary, then release it before holding
        // the observer boundary. Mutually observing agents can therefore never hold one another's
        // lifecycle guards. Observer setup validates this exact target generation before commit.
        let child_lifecycle_guard = state.acquire_live_agent_lifecycle(child_thread_id).await?;
        self.require_current_agent_ownership(child_thread_id)
            .await?;
        let child_thread = state.get_thread_including_pending(child_thread_id).await?;
        let child_lifecycle_generation = state.agent_lifecycle_generation(child_thread_id);
        drop(child_lifecycle_guard);
        let _parent_lifecycle_guard = state
            .acquire_live_agent_lifecycle(*parent_thread_id)
            .await?;
        let parent_thread = state
            .get_thread_including_pending(*parent_thread_id)
            .await?;
        let _transaction_permit = self
            .acquire_mailbox_submission_permit(child_thread_id)
            .await?;
        let _response_observation_transaction = self
            .acquire_response_observation_transaction(parent_thread.session.presentation_id())
            .await;
        self.ensure_v1_response_observer_for_thread(
            &state,
            &child_thread,
            parent_thread.session.presentation_id(),
            child_lifecycle_generation,
            response_observation,
            /*retain_passive_completion_relationship*/ false,
            ResponseObservationBinding::NextTurn,
            initial_terminal_observation,
            task_preview,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn ensure_v1_response_observer_for_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        child_thread: &Arc<CodexThread>,
        parent: SessionPresentationId,
        child_lifecycle_generation: u64,
        response_observation: ResponseObservationPolicy,
        retain_passive_completion_relationship: bool,
        binding: ResponseObservationBinding,
        initial_terminal_observation: InitialTerminalObservation,
        task_preview: Option<String>,
    ) -> CodexResult<AgentStatus> {
        let child_thread_id = child_thread.session.thread_id();
        let child = child_thread.session.presentation_id();
        validate_response_observation_endpoints(state, parent, child, child_lifecycle_generation)
            .await?;
        self.ensure_scoped_reply_route_supported(child_thread, response_observation)?;
        let parent_thread = state.get_thread_including_pending(parent.thread_id).await?;
        if child_thread_id == parent.thread_id {
            return Ok(child_thread.agent_status().await);
        }
        if !response_observation.commentary()
            && response_observation.final_response() == FinalResponseObservation::None
            && !response_observation.target_messages()
        {
            if binding == ResponseObservationBinding::NextTurn {
                let (response_snapshot, response_rx) =
                    child_thread.session.subscribe_agent_responses();
                drop(response_rx);
                let initial_reconciliation = initial_terminal_observation.reconcile(
                    response_snapshot.active_turn_id.clone(),
                    response_snapshot.last_terminal.clone(),
                    response_snapshot.status,
                );
                validate_response_observation_endpoints(
                    state,
                    parent,
                    child,
                    child_lifecycle_generation,
                )
                .await?;
                if !self
                    .persist_response_observation_updates(
                        parent,
                        self.response_observation_audit_snapshots(
                            parent,
                            child,
                            response_snapshot.active_turn_id,
                        ),
                    )
                    .await
                {
                    return Err(CodexErr::Fatal(
                        "failed to persist response observation audit state".to_string(),
                    ));
                }
                return Ok(initial_reconciliation.status);
            }
            return Ok(child_thread.agent_status().await);
        }
        let child_reference = self
            .get_agent_metadata(child_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .map_or_else(|| child_thread_id.to_string(), |path| path.to_string());
        let observes_future_turns = initial_terminal_observation.observes_future_turns();
        let retains_idle_next_turn = initial_terminal_observation.retains_idle_next_turn();
        let terminal_control = self.clone();
        let (response_snapshot, response_rx) = child_thread
            .session
            .subscribe_agent_responses_observing_terminal(move |turn_id, status| {
                if !terminal_control.agent_lifecycle_generation_is_current(
                    child.thread_id,
                    child_lifecycle_generation,
                ) {
                    return;
                }
                let _ = terminal_control.record_agent_terminal_presentation(
                    parent,
                    child,
                    turn_id,
                    status,
                    TerminalPresentationDelivery::Watcher,
                    || {},
                );
            });
        // A full-history fork can expose the parent's still-active turn in the new child snapshot.
        // Future-only observation begins after that snapshot boundary, so leave it pending until a
        // subsequent target turn is admitted instead of attaching it to inherited history.
        let mut target_turn_id = match binding {
            ResponseObservationBinding::NextTurn => initial_terminal_observation
                .target_turn_id(response_snapshot.active_turn_id.clone()),
            ResponseObservationBinding::ExplicitAdmission(_) => None,
        };
        let initial_reconciliation = initial_terminal_observation.reconcile(
            response_snapshot.active_turn_id.clone(),
            response_snapshot.last_terminal.clone(),
            response_snapshot.status.clone(),
        );
        if target_turn_id.is_none()
            && !observes_future_turns
            && let Some((turn_id, _)) = initial_reconciliation.terminal.as_ref()
        {
            // Live adoption can read Running immediately before this atomic subscription snapshot
            // observes the terminal outcome. Bind that reconciled turn so presentation-only
            // delivery is published instead of being mistaken for an idle bare-x audit tombstone.
            target_turn_id = Some(turn_id.clone());
        }
        if response_observation.final_response() == FinalResponseObservation::PresentationOnly
            && !response_observation.commentary()
            && target_turn_id.is_none()
            && !retains_idle_next_turn
        {
            validate_response_observation_endpoints(
                state,
                parent,
                child,
                child_lifecycle_generation,
            )
            .await?;
            if !self
                .persist_response_observation_updates(
                    parent,
                    self.response_observation_audit_snapshots(parent, child, target_turn_id),
                )
                .await
            {
                return Err(CodexErr::Fatal(
                    "failed to persist response observation audit state".to_string(),
                ));
            }
            return Ok(initial_reconciliation.status);
        }
        let previous_relationship = self.response_observation_relationship_snapshot(parent, child);
        let watcher_registration = self.register_response_watcher_with_admission_at_sequence(
            child,
            parent,
            &parent_thread.session.submission_admission,
            response_observation,
            retain_passive_completion_relationship,
            target_turn_id.clone(),
            binding,
            ResponseObservationPersistence::Durable,
            response_snapshot.next_event_sequence,
            response_snapshot.last_commentary_item_id,
        );
        if let Err(err) = validate_response_observation_endpoints(
            state,
            parent,
            child,
            child_lifecycle_generation,
        )
        .await
        {
            drop(watcher_registration);
            self.restore_response_observation_relationship_snapshot(
                parent,
                child,
                previous_relationship,
            );
            return Err(err);
        }
        if response_observation.target_messages()
            && let Some(target_turn_id) = target_turn_id.as_deref()
        {
            let route = self
                .with_agent_reply_route(
                    child_thread,
                    parent,
                    response_observation,
                    AgentControlInput::User(Vec::new()),
                )
                .await;
            let route_result = match route {
                Ok(route) => {
                    let Op::AgentInput {
                        items,
                        presentation,
                    } = route.into_op()
                    else {
                        unreachable!("reply-route context is core-authored agent input")
                    };
                    child_thread
                        .session
                        .steer_internal_agent_input(items, presentation, target_turn_id)
                        .await
                        .map(|_| ())
                        .map_err(|err| {
                            let message = match err {
                                SteerInputError::NoActiveTurn(_) => {
                                    "target turn completed before the reply route was delivered"
                                        .to_string()
                                }
                                SteerInputError::ActiveTurnPresent { actual } => {
                                    format!("unexpected active target turn `{actual}`")
                                }
                                SteerInputError::ExpectedTurnMismatch { expected, actual } => {
                                    format!(
                                        "expected target turn `{expected}` but found `{actual}`"
                                    )
                                }
                                SteerInputError::ActiveTurnNotSteerable { .. } => {
                                    "target turn does not accept reply-route input".to_string()
                                }
                                SteerInputError::EmptyInput => {
                                    "reply-route input was empty".to_string()
                                }
                            };
                            CodexErr::InvalidRequest(message)
                        })
                }
                Err(err) => Err(err),
            };
            if let Err(err) = route_result {
                drop(watcher_registration);
                self.restore_response_observation_relationship_snapshot(
                    parent,
                    child,
                    previous_relationship,
                );
                return Err(err);
            }
        }
        if let Some(task_preview) = task_preview.as_ref() {
            self.set_response_observation_task_preview(
                parent,
                child,
                target_turn_id.as_deref(),
                task_preview.clone(),
            );
        }
        if response_observation.exposes_source_model_context()
            && let Some(task_preview) = task_preview
            && let Err(err) = self
                .persist_user_agent_task_context(parent, child_thread_id, task_preview)
                .await
        {
            drop(watcher_registration);
            self.restore_response_observation_relationship_snapshot(
                parent,
                child,
                previous_relationship,
            );
            return Err(err);
        }
        if binding == ResponseObservationBinding::NextTurn
            && !self
                .persist_response_observation_snapshot(parent, child)
                .await
        {
            drop(watcher_registration);
            let message = "failed to persist response observation state";
            self.rollback_response_observation_relationship_locked(
                parent,
                child,
                previous_relationship,
                target_turn_id,
                message,
            )
            .await?;
            return Err(CodexErr::Fatal(message.to_string()));
        }
        let Some(watcher_registration) = watcher_registration else {
            return Ok(initial_reconciliation.status);
        };
        if let Some((turn_id, status)) = initial_reconciliation.terminal {
            let _ = self.record_agent_terminal_presentation(
                parent,
                child,
                &turn_id,
                status,
                TerminalPresentationDelivery::Watcher,
                || {},
            );
            // This subscription was created after the target turn finished, so there is no
            // ordered response event left to close an unanswered commentary admission.
            self.mark_response_observer_terminal_processed(parent, child, &turn_id);
        }
        self.start_v1_response_watcher(
            watcher_registration,
            response_rx,
            parent,
            child,
            child_reference,
        );
        Ok(initial_reconciliation.status)
    }

    fn start_v1_response_watcher(
        &self,
        watcher_registration: self::presentation::CompletionWatcherRegistration,
        mut response_rx: AgentResponseSubscription,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_reference: String,
    ) {
        let control = self.clone();
        tokio::spawn(async move {
            let child_lifecycle_generation = watcher_registration.child_lifecycle_generation();
            let mut watcher_guard = CompletionWatcherLifecycleGuard::new(
                control.clone(),
                watcher_registration,
                parent,
                child,
            );
            loop {
                let terminal = match control
                    .next_watcher_terminal(
                        parent,
                        child,
                        &mut response_rx,
                        MultiAgentVersion::V1,
                        child_lifecycle_generation,
                    )
                    .await
                {
                    WatcherTerminalPoll::Terminal(terminal) => terminal,
                    WatcherTerminalPoll::Retry => {
                        if !control.agent_lifecycle_generation_is_current(
                            child.thread_id,
                            child_lifecycle_generation,
                        ) || !control.response_observer_can_retry(parent).await
                        {
                            return;
                        }
                        while !control
                            .persist_response_observation_snapshot_transactionally(parent, child)
                            .await
                        {
                            if !control.agent_lifecycle_generation_is_current(
                                child.thread_id,
                                child_lifecycle_generation,
                            ) || !control.response_observer_can_retry(parent).await
                            {
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    WatcherTerminalPoll::Closed => {
                        if control.response_observer_can_retry(parent).await {
                            control.restart_v1_response_observer_after_runtime_end(
                                watcher_guard.take_registration(),
                                parent,
                                child,
                            );
                        }
                        return;
                    }
                };
                let status = terminal.status.clone();
                loop {
                    let Some(lifecycle_guard) = control
                        .acquire_current_agent_lifecycle(
                            child.thread_id,
                            child_lifecycle_generation,
                        )
                        .await
                    else {
                        return;
                    };
                    let delivered = control
                        .deliver_v1_watcher_terminal(
                            parent.thread_id,
                            child_reference.as_str(),
                            &terminal,
                            lifecycle_guard,
                        )
                        .await;
                    if delivered {
                        break;
                    }
                    if !control.agent_lifecycle_generation_is_current(
                        child.thread_id,
                        child_lifecycle_generation,
                    ) || !control
                        .terminal_response_observer_can_retry(parent, &terminal.presentation)
                        .await
                    {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                while !control
                    .finish_and_persist_response_observation_turn(parent, child, &terminal.turn_id)
                    .await
                {
                    if !control.agent_lifecycle_generation_is_current(
                        child.thread_id,
                        child_lifecycle_generation,
                    ) || !control.response_observer_can_retry(parent).await
                    {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if watcher_guard.retire_if_observation_idle() {
                    return;
                }
                if matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound) {
                    control.finish_watcher_terminal_presentation(parent, child, &terminal.turn_id);
                    control.restart_v1_response_observer_after_runtime_end(
                        watcher_guard.take_registration(),
                        parent,
                        child,
                    );
                    return;
                }
            }
        });
    }

    pub(super) fn restart_v1_response_observer_after_runtime_end(
        &self,
        watcher_registration: Option<self::presentation::CompletionWatcherRegistration>,
        parent: SessionPresentationId,
        child: SessionPresentationId,
    ) {
        self.schedule_v1_response_observer_restore(
            watcher_registration,
            parent,
            child,
            Some(child),
        );
    }

    fn schedule_v1_response_observer_restore(
        &self,
        watcher_registration: Option<self::presentation::CompletionWatcherRegistration>,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        previous_child: Option<SessionPresentationId>,
    ) {
        let Some(mut watcher_registration) = watcher_registration else {
            return;
        };
        let child_lifecycle_generation = watcher_registration.child_lifecycle_generation();
        if !self.agent_lifecycle_generation_is_current(child.thread_id, child_lifecycle_generation)
        {
            // Dropping the stale registration revokes this control tree's relationship directly,
            // before recovery can use the per-presentation revocation path below.
            let _watcher_guard = CompletionWatcherLifecycleGuard::new(
                self.clone(),
                watcher_registration,
                parent,
                child,
            );
            return;
        }
        let observations = self.response_observation_snapshots(parent, child);
        if !response_observations_have_work(&observations) {
            return;
        }
        watcher_registration.preserve_state_for_replacement_on_drop();
        drop(watcher_registration);

        // Watcher exits can race transient durability/admission failures or child reload. Keep the
        // relationship alive within this orchestration instance and reconstruct from canonical
        // child history instead of silently unsubscribing when this watcher registration is
        // dropped. Cold resume and fork deliberately do not call this recovery path.
        let control = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            control
                .restore_v1_response_observer(
                    parent,
                    child.thread_id,
                    child_lifecycle_generation,
                    previous_child,
                    observations,
                )
                .await;
        });
    }

    pub(super) async fn restore_v1_response_observer(
        &self,
        parent: SessionPresentationId,
        target_thread_id: ThreadId,
        target_lifecycle_generation: u64,
        previous_child: Option<SessionPresentationId>,
        observations: Vec<codex_protocol::protocol::AgentResponseObservation>,
    ) {
        if !self.response_observer_generation_is_current(
            parent,
            target_thread_id,
            target_lifecycle_generation,
            previous_child,
        ) {
            return;
        }
        let Ok(state) = self.upgrade() else {
            return;
        };
        let Ok(observer_thread) = state.get_thread_including_pending(parent.thread_id).await else {
            self.revoke_previous_response_observation(parent, previous_child);
            return;
        };
        if observer_thread.session.presentation_id() != parent
            || !observer_thread
                .session
                .submission_admission
                .response_observation_delivery_can_retry()
        {
            self.revoke_previous_response_observation(parent, previous_child);
            return;
        }
        let observer_termination = observer_thread.io.session_loop_termination.clone();
        drop(observer_thread);
        let mut thread_created = state.subscribe_thread_created();
        let live_child = state
            .get_thread(target_thread_id)
            .await
            .ok()
            .filter(|child_thread| {
                previous_child
                    .is_none_or(|previous| child_thread.session.presentation_id() != previous)
            });
        let live_subscription = live_child.as_ref().map(|child_thread| {
            let child = child_thread.session.presentation_id();
            let terminal_control = self.clone();
            let (response_snapshot, response_rx) = child_thread
                .session
                .subscribe_agent_responses_observing_terminal(move |turn_id, status| {
                    if !terminal_control.agent_lifecycle_generation_is_current(
                        child.thread_id,
                        target_lifecycle_generation,
                    ) {
                        return;
                    }
                    let _ = terminal_control.record_agent_terminal_presentation(
                        parent,
                        child,
                        turn_id,
                        status,
                        TerminalPresentationDelivery::Watcher,
                        || {},
                    );
                });
            (child, response_snapshot, response_rx)
        });
        let mut history_retry_delay = std::time::Duration::from_millis(100);
        let recovered_events = loop {
            if !self.response_observer_generation_is_current(
                parent,
                target_thread_id,
                target_lifecycle_generation,
                previous_child,
            ) {
                return;
            }
            if !self.response_observer_can_retry(parent).await {
                self.revoke_previous_response_observation(parent, previous_child);
                return;
            }
            match state
                .load_canonical_thread_history(LoadThreadHistoryParams {
                    thread_id: target_thread_id,
                    include_archived: true,
                })
                .await
            {
                Ok(history) => {
                    let canonical_items = rollout_without_exact_rollback_ranges(&history.items);
                    break agent_response_events_from_rollout(&canonical_items);
                }
                Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                    break Vec::new();
                }
                Err(err) => {
                    warn!(
                        observer_thread_id = %parent.thread_id,
                        %target_thread_id,
                        "failed to load canonical response-observation target history; retrying: {err}"
                    );
                    tokio::select! {
                        () = observer_termination.clone() => {
                            self.revoke_previous_response_observation(parent, previous_child);
                            return;
                        },
                        () = tokio::time::sleep(history_retry_delay) => {}
                    }
                    history_retry_delay = history_retry_delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(2));
                }
            }
        };
        let observes_recovered_event = observations
            .iter()
            .any(|observation| observation.commentary_delivery.is_some())
            || recovered_events.iter().any(|event| match event {
                crate::session::AgentResponseEvent::Commentary { turn_id, .. } => {
                    observations.iter().any(|observation| {
                        observation.target_turn_id.as_deref() == Some(turn_id.as_str())
                            && (observation.pending_commentary
                                || !observation.commentary_after_sequences.is_empty()
                                || !observation.commentary_admissions.is_empty())
                    })
                }
                crate::session::AgentResponseEvent::Terminal { turn_id, .. } => {
                    observations.iter().any(|observation| {
                        observation.target_turn_id.as_deref() == Some(turn_id.as_str())
                    })
                }
                crate::session::AgentResponseEvent::TurnStarted { .. }
                | crate::session::AgentResponseEvent::TurnAborted { .. } => false,
            });
        if !self.response_observer_generation_is_current(
            parent,
            target_thread_id,
            target_lifecycle_generation,
            previous_child,
        ) {
            return;
        }
        let (child, response_snapshot, response_rx, recovered_only) = if let Some((
            child,
            response_snapshot,
            response_rx,
        )) = live_subscription
        {
            if live_child
                .as_ref()
                .is_some_and(|child_thread| child_thread.session.thread_id == parent.thread_id)
            {
                self.revoke_previous_response_observation(parent, previous_child);
                return;
            }
            (child, Some(response_snapshot), Some(response_rx), false)
        } else if observes_recovered_event {
            (
                SessionPresentationId::new(target_thread_id, uuid::Uuid::nil()),
                None,
                None,
                true,
            )
        } else {
            warn!(
                observer_thread_id = %parent.thread_id,
                %target_thread_id,
                "response observation target is not currently loaded; waiting for UUID-based reload"
            );
            loop {
                if !self.response_observer_generation_is_current(
                    parent,
                    target_thread_id,
                    target_lifecycle_generation,
                    previous_child,
                ) {
                    return;
                }
                if let Ok(child_thread) = state.get_thread_including_pending(target_thread_id).await
                    && previous_child
                        .is_none_or(|previous| child_thread.session.presentation_id() != previous)
                {
                    Box::pin(self.restore_v1_response_observer(
                        parent,
                        target_thread_id,
                        target_lifecycle_generation,
                        previous_child,
                        observations,
                    ))
                    .await;
                    return;
                }
                tokio::select! {
                    () = observer_termination.clone() => {
                        self.revoke_previous_response_observation(parent, previous_child);
                        return;
                    },
                    () = state.wait_for_agent_lifecycle_change() => {}
                    () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                    created = thread_created.recv() => match created {
                        Ok(thread_id) if thread_id == target_thread_id => {}
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        };
        let Ok(current_observer) = state.get_thread_including_pending(parent.thread_id).await
        else {
            self.revoke_previous_response_observation(parent, previous_child);
            return;
        };
        if current_observer.session.presentation_id() != parent {
            self.revoke_previous_response_observation(parent, previous_child);
            return;
        }
        let Some(lifecycle_guard) = self
            .acquire_current_agent_lifecycle(target_thread_id, target_lifecycle_generation)
            .await
        else {
            if self.response_observer_generation_is_current(
                parent,
                target_thread_id,
                target_lifecycle_generation,
                previous_child,
            ) {
                Box::pin(self.restore_v1_response_observer(
                    parent,
                    target_thread_id,
                    target_lifecycle_generation,
                    previous_child,
                    observations,
                ))
                .await;
            }
            return;
        };
        let child_reference = self
            .get_agent_metadata(child.thread_id)
            .and_then(|metadata| metadata.agent_path)
            .map_or_else(|| child.thread_id.to_string(), |path| path.to_string());
        let mut watcher_registration = None;
        {
            let _transaction_permit = self.acquire_response_observation_transaction(parent).await;
            // A send/resume may have attached a newer live watcher while this recovery task was
            // loading canonical history. Its relationship state is authoritative.
            if self.has_completion_watcher(parent, child) {
                if let Some(previous_child) = previous_child {
                    // Child selection above excludes the previous presentation. Clearing it
                    // cannot remove the newer watcher relationship that caused this early return.
                    debug_assert_ne!(previous_child, child);
                    self.clear_response_observation_relationship(parent, previous_child);
                }
                return;
            }
            for observation in &observations {
                let registration = self.restore_response_watcher_with_admission(
                    child,
                    parent,
                    &current_observer.session.submission_admission,
                    observation,
                );
                if watcher_registration.is_none() {
                    watcher_registration = registration;
                }
            }
            if let Some(previous_child) = previous_child {
                debug_assert_ne!(previous_child, child);
                self.clear_response_observation_relationship(parent, previous_child);
            }
        }
        drop(lifecycle_guard);
        // Replaying recovered output can await persistence and destination delivery. Guard the
        // registration during that window so every permanent early exit both revokes its bound
        // wake and rechecks an idle observer, just like the eventual live watcher task.
        let mut watcher_guard = watcher_registration.map(|watcher_registration| {
            CompletionWatcherLifecycleGuard::new(self.clone(), watcher_registration, parent, child)
        });

        for turn_id in observations
            .iter()
            .filter_map(|observation| observation.target_turn_id.as_deref())
        {
            if let Some(delivery) =
                self.response_observation_commentary_delivery(parent, child, turn_id)
            {
                loop {
                    let Some(lifecycle_guard) = self
                        .acquire_current_agent_lifecycle(
                            target_thread_id,
                            target_lifecycle_generation,
                        )
                        .await
                    else {
                        return;
                    };
                    let delivered = self
                        .deliver_v1_commentary_observation(
                            parent,
                            child,
                            turn_id,
                            &delivery,
                            lifecycle_guard,
                        )
                        .await;
                    if delivered {
                        break;
                    }
                    if !self.agent_lifecycle_generation_is_current(
                        target_thread_id,
                        target_lifecycle_generation,
                    ) || !self.response_observer_can_retry(parent).await
                    {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }

        let mut stop_watcher = false;
        let mut prior_commentary_item_ids = HashMap::<String, HashSet<String>>::new();
        for event in recovered_events {
            match event {
                crate::session::AgentResponseEvent::Commentary {
                    turn_id,
                    item_id,
                    text,
                    sequence,
                } if observations.iter().any(|observation| {
                    observation.target_turn_id.as_deref() == Some(turn_id.as_str())
                }) =>
                {
                    let prior_item_ids = prior_commentary_item_ids
                        .entry(turn_id.clone())
                        .or_default();
                    loop {
                        let Some(lifecycle_guard) = self
                            .acquire_current_agent_lifecycle(
                                target_thread_id,
                                target_lifecycle_generation,
                            )
                            .await
                        else {
                            return;
                        };
                        let delivered = self
                            .deliver_recovered_v1_commentary(
                                parent,
                                child,
                                &turn_id,
                                &item_id,
                                &text,
                                sequence,
                                prior_item_ids,
                                lifecycle_guard,
                            )
                            .await;
                        if delivered {
                            break;
                        }
                        if !self.agent_lifecycle_generation_is_current(
                            target_thread_id,
                            target_lifecycle_generation,
                        ) || !self.response_observer_can_retry(parent).await
                        {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    prior_item_ids.insert(item_id);
                }
                crate::session::AgentResponseEvent::Terminal { turn_id, status }
                    if observations.iter().any(|observation| {
                        observation.target_turn_id.as_deref() == Some(turn_id.as_str())
                    }) =>
                {
                    while !self
                        .reconcile_restored_v1_terminal(
                            parent,
                            child,
                            target_lifecycle_generation,
                            &child_reference,
                            &turn_id,
                            status.clone(),
                        )
                        .await
                    {
                        if !self.agent_lifecycle_generation_is_current(
                            target_thread_id,
                            target_lifecycle_generation,
                        ) || !self.response_observer_can_retry(parent).await
                        {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    stop_watcher |= matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound);
                }
                crate::session::AgentResponseEvent::TurnStarted { .. }
                | crate::session::AgentResponseEvent::Commentary { .. }
                | crate::session::AgentResponseEvent::TurnAborted { .. }
                | crate::session::AgentResponseEvent::Terminal { .. } => {}
            }
        }

        if let Some(response_snapshot) = response_snapshot {
            let inferred_terminal_turn_id = || {
                let mut target_turn_ids = observations
                    .iter()
                    .filter_map(|observation| observation.target_turn_id.as_deref());
                let turn_id = target_turn_ids.next()?;
                target_turn_ids
                    .next()
                    .is_none()
                    .then(|| turn_id.to_string())
            };
            let terminal = response_snapshot.last_terminal.or_else(|| {
                crate::agent::status::is_final(&response_snapshot.status)
                    .then(inferred_terminal_turn_id)
                    .flatten()
                    .map(|turn_id| (turn_id, response_snapshot.status.clone()))
            });
            if let Some((turn_id, status)) = terminal {
                while !self
                    .reconcile_restored_v1_terminal(
                        parent,
                        child,
                        target_lifecycle_generation,
                        &child_reference,
                        &turn_id,
                        status.clone(),
                    )
                    .await
                {
                    if !self.agent_lifecycle_generation_is_current(
                        target_thread_id,
                        target_lifecycle_generation,
                    ) || !self.response_observer_can_retry(parent).await
                    {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                stop_watcher |= matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound);
            }
        }

        if recovered_only {
            let remaining_observations = self.response_observation_snapshots(parent, child);
            if !response_observations_have_work(&remaining_observations) {
                return;
            }
            // Keep the recovered relationship visible while waiting for a live replacement. A
            // bound wake must continue suppressing idle automation across this runtime-only gap.
            let mut watcher_registration = watcher_guard
                .as_mut()
                .and_then(CompletionWatcherLifecycleGuard::take_registration);
            if let Some(watcher_registration) = watcher_registration.as_mut() {
                watcher_registration.preserve_state_for_replacement_on_drop();
            }
            drop(watcher_registration);
            let previous_child = Some(child);
            loop {
                if !self.response_observer_generation_is_current(
                    parent,
                    target_thread_id,
                    target_lifecycle_generation,
                    previous_child,
                ) {
                    return;
                }
                if let Ok(child_thread) = state.get_thread_including_pending(target_thread_id).await
                    && previous_child
                        .is_none_or(|previous| child_thread.session.presentation_id() != previous)
                {
                    Box::pin(self.restore_v1_response_observer(
                        parent,
                        target_thread_id,
                        target_lifecycle_generation,
                        previous_child,
                        remaining_observations,
                    ))
                    .await;
                    return;
                }
                tokio::select! {
                    () = observer_termination.clone() => {
                        self.revoke_previous_response_observation(parent, previous_child);
                        return;
                    },
                    () = state.wait_for_agent_lifecycle_change() => {}
                    () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                    created = thread_created.recv() => match created {
                        Ok(thread_id) if thread_id == target_thread_id => {}
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
        if stop_watcher {
            self.restart_v1_response_observer_after_runtime_end(
                watcher_guard
                    .as_mut()
                    .and_then(CompletionWatcherLifecycleGuard::take_registration),
                parent,
                child,
            );
            return;
        }
        if let Some(response_rx) = response_rx
            && let Some(watcher_registration) = watcher_guard
                .as_mut()
                .and_then(CompletionWatcherLifecycleGuard::take_registration)
        {
            self.start_v1_response_watcher(
                watcher_registration,
                response_rx,
                parent,
                child,
                child_reference,
            );
        }
    }

    async fn reconcile_restored_v1_terminal(
        &self,
        parent: SessionPresentationId,
        child: SessionPresentationId,
        child_lifecycle_generation: u64,
        child_reference: &str,
        turn_id: &str,
        status: AgentStatus,
    ) -> bool {
        if !self
            .response_observation_snapshots(parent, child)
            .iter()
            .any(|observation| observation.target_turn_id.as_deref() == Some(turn_id))
        {
            return true;
        }
        let terminal = self
            .take_watcher_terminal_presentation(parent, child)
            .or_else(|| {
                // Recovery may revisit a final turn outcome whose original watcher was lost after
                // the runtime dedup marker was set but before its queue survived. Clear only that
                // marker; the committed response-item ID remains the durable delivery guard.
                self.finish_watcher_terminal_presentation(parent, child, turn_id);
                let _ = self.record_agent_terminal_presentation(
                    parent,
                    child,
                    turn_id,
                    status.clone(),
                    TerminalPresentationDelivery::Watcher,
                    || {},
                );
                self.take_watcher_terminal_presentation(parent, child)
            });
        if let Some(terminal) = terminal {
            self.mark_response_observer_terminal_processed(parent, child, turn_id);
            loop {
                let Some(lifecycle_guard) = self
                    .acquire_current_agent_lifecycle(child.thread_id, child_lifecycle_generation)
                    .await
                else {
                    return false;
                };
                let delivered = self
                    .deliver_v1_watcher_terminal(
                        parent.thread_id,
                        child_reference,
                        &terminal,
                        lifecycle_guard,
                    )
                    .await;
                if delivered {
                    break;
                }
                if !self.agent_lifecycle_generation_is_current(
                    child.thread_id,
                    child_lifecycle_generation,
                ) || !self
                    .terminal_response_observer_can_retry(parent, &terminal.presentation)
                    .await
                {
                    return false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            while !self
                .finish_and_persist_response_observation_turn(parent, child, turn_id)
                .await
            {
                if !self.agent_lifecycle_generation_is_current(
                    child.thread_id,
                    child_lifecycle_generation,
                ) || !self.response_observer_can_retry(parent).await
                {
                    return false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        if matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound) {
            self.finish_watcher_terminal_presentation(parent, child, turn_id);
        }
        true
    }
}
