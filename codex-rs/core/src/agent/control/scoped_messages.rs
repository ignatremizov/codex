use super::*;
use crate::context::AgentReplyRoute;
use crate::context::AttributedAgentMessage;
use crate::session::SteerInputError;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;

#[derive(Clone, Copy)]
enum AgentReplyRouteLifetime {
    CurrentTurn,
    UntilDisabled,
}

impl AgentControl {
    /// Capture trusted send-time attribution without changing admission or reply permission.
    pub(crate) async fn attribute_model_input(
        &self,
        sender: SessionPresentationId,
        recipient: ThreadId,
        sender_turn_id: &str,
        input: Vec<UserInput>,
    ) -> CodexResult<AgentControlInput> {
        let state = self.upgrade()?;
        let sender_thread = state.get_thread_including_pending(sender.thread_id).await?;
        if sender_thread.session.presentation_id() != sender {
            return Err(CodexErr::ThreadNotFound(sender.thread_id));
        }
        let (sender_context, sender_identity) = self.agent_input_identity(sender.thread_id).await?;
        let (_, recipient_identity) = self.agent_input_identity(recipient).await?;
        // The original typed items, including attachment metadata and text-element spans,
        // remain in the durable presentation. Only model-facing text is enveloped.
        let message = render_input_preview(&input);
        let mut content = vec![UserInput::Text {
            text: AttributedAgentMessage::new(sender_context, message).render(),
            text_elements: Vec::new(),
        }];
        content.extend(
            input
                .iter()
                .filter(|item| !matches!(item, UserInput::Text { .. }))
                .cloned(),
        );
        Ok(AgentControlInput::AttributedAgentInput {
            content,
            attribution: Box::new(AgentInputAttribution {
                sender: sender_identity,
                recipient: recipient_identity,
                sender_turn_id: sender_turn_id.to_string(),
            }),
            presentation: input,
        })
    }

    async fn agent_input_identity(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<(AgentContextIdentity, AgentInputIdentity)> {
        let identity = self
            .model_visible_agent_identity_for_version(MultiAgentVersion::V1, thread_id)
            .await?;
        let AgentContextIdentity::V1 {
            agent_ref,
            nickname,
            task_path,
            ..
        } = &identity
        else {
            unreachable!("V1 identity resolution always returns a V1 identity")
        };
        let snapshot = self.get_agent_config_snapshot(thread_id).await;
        let metadata = self.get_agent_metadata(thread_id);
        let audit = AgentInputIdentity {
            thread_id,
            nickname: nickname.clone(),
            agent_ref: agent_ref.map(|agent_ref| agent_ref.to_string()),
            task_path: task_path.clone(),
            role: snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.session_source.get_agent_role())
                .or_else(|| metadata.and_then(|metadata| metadata.agent_role)),
            model: snapshot.as_ref().map(|snapshot| snapshot.model.clone()),
            reasoning_effort: snapshot.and_then(|snapshot| snapshot.reasoning_effort),
        };
        Ok((identity, audit))
    }

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

    pub(super) fn ensure_target_message_route_allowed(
        &self,
        target_thread: &CodexThread,
        observer: SessionPresentationId,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<()> {
        self.ensure_scoped_reply_route_supported(target_thread, response_observation)?;
        if response_observation.target_messages()
            && self.target_message_route_mode(observer, target_thread.session.presentation_id())
                == Some(TargetMessageRouteMode::Disabled)
        {
            return Err(CodexErr::InvalidRequest(format!(
                "agent replies to {} are disabled by the user",
                observer.thread_id
            )));
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
        self.ensure_target_message_route_allowed(target_thread, observer, response_observation)?;
        let target = target_thread.session.presentation_id();
        match self.target_message_route_mode(observer, target) {
            Some(TargetMessageRouteMode::Disabled) => unreachable!(
                "disabled reply routes are rejected before constructing target context"
            ),
            Some(TargetMessageRouteMode::Enabled) => return Ok(input),
            None => {}
        }
        let route = self
            .agent_reply_route_item(
                target_thread,
                observer,
                AgentReplyRouteLifetime::CurrentTurn,
            )
            .await?;
        let ResponseItem::Message { content, .. } = route else {
            unreachable!("agent reply routes are contextual user messages")
        };
        let Some(ContentItem::InputText { text }) = content.into_iter().next() else {
            unreachable!("agent reply routes contain one text item")
        };
        input.push_internal_context(UserInput::Text {
            text,
            text_elements: Vec::new(),
        });
        Ok(input)
    }

    async fn agent_reply_route_item(
        &self,
        target_thread: &CodexThread,
        observer: SessionPresentationId,
        lifetime: AgentReplyRouteLifetime,
    ) -> CodexResult<ResponseItem> {
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
        Ok(match lifetime {
            AgentReplyRouteLifetime::CurrentTurn => {
                ContextualUserFragment::into(AgentReplyRoute::new(agent))
            }
            AgentReplyRouteLifetime::UntilDisabled => {
                ContextualUserFragment::into(AgentReplyRoute::until_disabled(agent))
            }
        })
    }

    pub(super) async fn inject_agent_reply_route(
        &self,
        target_thread: &CodexThread,
        observer: SessionPresentationId,
        target_turn_id: &str,
    ) -> CodexResult<()> {
        match self.target_message_route_mode(observer, target_thread.session.presentation_id()) {
            Some(TargetMessageRouteMode::Disabled) => {
                return Err(CodexErr::InvalidRequest(format!(
                    "agent replies to {} are disabled by the user",
                    observer.thread_id
                )));
            }
            Some(TargetMessageRouteMode::Enabled) => return Ok(()),
            None => {}
        }
        let response_observation = ResponseObservationPolicy::from_turn_parts(
            /*commentary*/ false,
            FinalResponseObservation::None,
            /*target_messages*/ true,
            /*queue_input*/ false,
        );
        let route = self
            .with_agent_reply_route(
                target_thread,
                observer,
                response_observation,
                AgentControlInput::User(Vec::new()),
            )
            .await?;
        let Op::AgentInput {
            items,
            presentation,
        } = route.into_op()
        else {
            unreachable!("reply-route context is core-authored agent input")
        };
        target_thread
            .session
            .steer_internal_agent_input(items, presentation, target_turn_id)
            .await
            .map(|_| ())
            .map_err(|err| {
                let message = match err {
                    SteerInputError::NoActiveTurn(_) => {
                        "target turn completed before the reply route was delivered".to_string()
                    }
                    SteerInputError::ActiveTurnPresent { actual } => {
                        format!("unexpected active target turn `{actual}`")
                    }
                    SteerInputError::ExpectedTurnMismatch { expected, actual } => {
                        format!("expected target turn `{expected}` but found `{actual}`")
                    }
                    SteerInputError::ActiveTurnNotSteerable { .. } => {
                        "target turn does not accept reply-route input".to_string()
                    }
                    SteerInputError::EmptyInput => "reply-route input was empty".to_string(),
                };
                CodexErr::InvalidRequest(message)
            })
    }

    pub(super) async fn prepare_persistent_agent_reply_route(
        &self,
        target_thread: &CodexThread,
        observer: SessionPresentationId,
    ) -> CodexResult<ResponseItem> {
        self.agent_reply_route_item(
            target_thread,
            observer,
            AgentReplyRouteLifetime::UntilDisabled,
        )
        .await
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
        self.refresh_subtree_messaging(target.thread_id).await?;
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
        let attributed_input = receiver_control
            .attribute_model_input(sender, receiver_thread_id, sender_turn_id, input)
            .await?;
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
            let (admission_mode, scoped_authorization) = match admission {
                TargetMessageAdmission::Steer => (
                    InputTurnAdmissionMode::SteerOnly(SteerTargetTurn::Current),
                    ScopedInputAuthorization::Steer {
                        receiver,
                        sender_turn_id: sender_turn_id.to_string(),
                    },
                ),
                TargetMessageAdmission::Wake(reservation_id) => (
                    InputTurnAdmissionMode::AnyTurn,
                    ScopedInputAuthorization::Wake {
                        receiver,
                        sender_turn_id: sender_turn_id.to_string(),
                        reservation_id,
                    },
                ),
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
                        scoped_authorization,
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
