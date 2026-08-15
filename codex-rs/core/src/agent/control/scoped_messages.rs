use super::*;
use crate::context::AgentReplyRoute;
use crate::context::AttributedAgentMessage;

impl AgentControl {
    pub(super) fn ensure_scoped_reply_route_supported(
        &self,
        target_thread: &CodexThread,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<()> {
        if response_observation.target_messages()
            && target_thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        {
            return Err(CodexErr::UnsupportedOperation(
                "this target does not support scoped reply routes; omit m".to_string(),
            ));
        }
        Ok(())
    }

    pub(super) async fn with_agent_reply_route(
        &self,
        target_thread: &CodexThread,
        observer: SessionPresentationId,
        response_observation: ResponseObservationPolicy,
        mut input: AgentControlInput,
    ) -> CodexResult<AgentControlInput> {
        if !response_observation.target_messages() {
            return Ok(input);
        }
        self.ensure_scoped_reply_route_supported(target_thread, response_observation)?;
        if target_thread.session.presentation_id().thread_id == observer.thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot grant a reply route to itself".to_string(),
            ));
        }
        let state = self.upgrade()?;
        let observer_thread = state
            .get_thread_including_pending(observer.thread_id)
            .await?;
        if observer_thread.session.presentation_id() != observer {
            return Err(CodexErr::InvalidRequest(
                "agent response observer is no longer current".to_string(),
            ));
        }
        let agent = self
            .model_visible_agent_identity(&observer_thread, observer.thread_id)
            .await?;
        input.push_internal_context(UserInput::Text {
            text: AgentReplyRoute::new(agent).render(),
            text_elements: Vec::new(),
        });
        Ok(input)
    }

    pub(super) async fn acquire_target_message_admission_after_binding(
        &self,
        observer_thread: &CodexThread,
        observer: SessionPresentationId,
        target_thread: &CodexThread,
        target: SessionPresentationId,
        target_turn_id: &str,
        mode: TargetMessageAdmissionMode,
    ) -> CodexResult<TargetMessageAdmission> {
        loop {
            let changed = self.response_observation_changed().notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let transaction = self
                .acquire_response_observation_transaction(observer)
                .await;
            if target_thread.session.presentation_id() != target
                || target_thread
                    .session
                    .active_agent_response_turn_id()
                    .as_deref()
                    != Some(target_turn_id)
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "agent message route is not active for sender turn {target_turn_id}"
                )));
            }
            let (observer_snapshot, observer_subscription) =
                observer_thread.session.subscribe_agent_responses();
            drop(observer_subscription);
            match self.target_message_admission(
                observer,
                target,
                target_turn_id,
                observer_snapshot.active_turn_id.as_deref(),
                observer_snapshot
                    .last_terminal
                    .as_ref()
                    .map(|(turn_id, _status)| turn_id.as_str()),
                mode,
            ) {
                Ok(admission) => return Ok(admission),
                Err(_err) if self.target_message_binding_pending(observer, target) => {
                    drop(transaction);
                    changed.as_mut().await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub(crate) async fn send_scoped_agent_input_observing_response(
        &self,
        sender: SessionPresentationId,
        sender_turn_id: &str,
        receiver_thread_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let receiver_lifecycle_lock = state.agent_lifecycle_lock(receiver_thread_id);
        let _receiver_lifecycle_guard = receiver_lifecycle_lock.lock_owned().await;
        let receiver_thread = state
            .get_thread_including_pending(receiver_thread_id)
            .await?;
        let receiver = receiver_thread.session.presentation_id();
        let receiver_control = receiver_thread.session.services.agent_control.clone();
        receiver_control
            .require_current_agent_ownership(receiver_thread_id)
            .await?;

        let sender_thread = state.get_thread_including_pending(sender.thread_id).await?;
        let sender_identity = receiver_control
            .model_visible_agent_identity(&receiver_thread, sender.thread_id)
            .await?;
        let attributed_input = attributed_agent_input(sender_identity, sender_turn_id, input);
        loop {
            let admission = receiver_control
                .acquire_target_message_admission_after_binding(
                    &receiver_thread,
                    receiver,
                    &sender_thread,
                    sender,
                    sender_turn_id,
                    TargetMessageAdmissionMode::SteerOrWake,
                )
                .await?;
            let admission_mode = match admission {
                TargetMessageAdmission::Steer => InputTurnAdmissionMode::SteerOnly,
                TargetMessageAdmission::Wake(_) => InputTurnAdmissionMode::AnyTurn,
                TargetMessageAdmission::PendingWake => {
                    return Err(CodexErr::InvalidRequest(
                        "agent message wake is still being admitted; retry this message"
                            .to_string(),
                    ));
                }
            };
            let submission = receiver_control
                .send_input_observing_response_to_retained_thread_locked(
                    receiver_thread_id,
                    &state,
                    &receiver_thread,
                    ObservedInputAdmission {
                        input: attributed_input.clone(),
                        start_options: start_options.clone(),
                        observer: sender,
                        response_observation,
                        admission_mode,
                        task_context: ObservedInputTaskContext::None,
                    },
                )
                .await;
            let mut submission = match submission {
                Ok(submission) => submission,
                Err(err) => {
                    if let TargetMessageAdmission::Wake(reservation_id) = admission {
                        receiver_control.rollback_target_message_wake_reservation(
                            receiver,
                            sender,
                            sender_turn_id,
                            reservation_id,
                        );
                    }
                    if admission == TargetMessageAdmission::Steer
                        && is_steer_only_target_ended_error(&err)
                    {
                        continue;
                    }
                    return Err(err);
                }
            };
            let wake_committed = match admission {
                TargetMessageAdmission::Wake(reservation_id) => receiver_control
                    .commit_target_message_wake(
                        receiver,
                        sender,
                        sender_turn_id,
                        reservation_id,
                        &submission.target_turn_id,
                    ),
                TargetMessageAdmission::Steer | TargetMessageAdmission::PendingWake => false,
            };
            if wake_committed
                && !receiver_control
                    .persist_response_observation_snapshot(receiver, sender)
                    .await
            {
                let warning =
                    "agent message was admitted, but its one-wake state could not be persisted";
                tracing::warn!(
                    observer_thread_id = %receiver.thread_id,
                    target_thread_id = %sender.thread_id,
                    target_turn_id = sender_turn_id,
                    wake_turn_id = submission.target_turn_id,
                    warning
                );
                submission.post_admission_warning =
                    Some(submission.post_admission_warning.map_or_else(
                        || warning.to_string(),
                        |existing| format!("{existing}; {warning}"),
                    ));
            }
            if wake_committed {
                let (snapshot, subscription) = receiver_thread.session.subscribe_agent_responses();
                drop(subscription);
                if snapshot.active_turn_id.as_deref() != Some(&submission.target_turn_id)
                    && snapshot
                        .last_terminal
                        .as_ref()
                        .is_some_and(|(turn_id, _)| turn_id == &submission.target_turn_id)
                {
                    receiver_control
                        .finish_target_message_wake(receiver, &submission.target_turn_id);
                }
            }
            return submission.into_strict_result();
        }
    }
}

pub(super) fn attributed_agent_input(
    sender: AgentContextIdentity,
    sender_turn_id: &str,
    input: Vec<UserInput>,
) -> AgentControlInput {
    // An explicit `m` grant promotes complete agent-to-agent input. Preserve its payload just like
    // a successful child completion; generic command-output and error truncation do not apply.
    let message = render_input_preview(&input);
    let presentation = input.clone();
    let agent_id = match &sender {
        AgentContextIdentity::V1 { agent_id, .. }
        | AgentContextIdentity::V2 { agent_id, .. }
        | AgentContextIdentity::Canonical { agent_id } => *agent_id,
    };
    let transcript = format!("Agent message from `{agent_id}`:\n\n{message}");
    let mut content = vec![UserInput::Text {
        text: AttributedAgentMessage::new(sender, sender_turn_id, message).render(),
        text_elements: Vec::new(),
    }];
    content.extend(
        input
            .into_iter()
            .filter(|item| !matches!(item, UserInput::Text { .. })),
    );
    AgentControlInput::AttributedAgent {
        content,
        transcript,
        presentation,
    }
}
