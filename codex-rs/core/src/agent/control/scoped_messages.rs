use super::*;
use crate::CodexThread;
use crate::context::AgentReplyRoute;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(super) enum AgentReplyRouteLifetime {
    CurrentTurn,
    UntilDisabled,
}

impl LocalAgentControl {
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
            Some(TargetMessageRouteMode::Disabled) => {
                return Err(CodexErr::InvalidRequest(
                    "agent replies are disabled by the user".into(),
                ));
            }
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

    pub(super) async fn agent_reply_route_item(
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
        let observer_thread = state.get_thread(observer.thread_id).await?;
        if observer_thread.session.presentation_id() != observer {
            return Err(CodexErr::InvalidRequest(
                "agent response observer is no longer current".to_string(),
            ));
        }
        let agent = self
            .model_visible_agent_identity_for_version(
                target_thread
                    .multi_agent_version()
                    .unwrap_or(MultiAgentVersion::V1),
                observer.thread_id,
            )
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
        origin: AgentModelInputOrigin,
        batch_id: Option<&str>,
        receiver_thread_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<String> {
        let control = self.clone();
        let AgentModelInputOrigin {
            sender,
            sender_turn_id,
        } = origin;
        let batch_id = batch_id.map(str::to_string);
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let lifecycle = state.acquire_live_agent_lifecycle(receiver_thread_id).await?;
            let receiver_thread = state.get_thread(receiver_thread_id).await?;
            let receiver = receiver_thread.session.presentation_id();
            let receiver_control = receiver_thread.session.services.agent_control.clone();
            receiver_control.require_current_agent_ownership(receiver_thread_id).await?;
            receiver_control.ensure_scoped_reply_route_supported(&receiver_thread, response_observation)?;
            let source_guard = state.agent_turn_queue.acquire_source_admission(sender.thread_id).await;
            let sender_thread = state.get_thread(sender.thread_id).await?;
            sender_thread.session.submission_admission.check_ready()?;
            receiver_control.refresh_subtree_messaging(receiver_thread_id).await?;
            let input = receiver_control
                .attribute_model_input(
                    sender,
                    receiver_thread_id,
                    &sender_turn_id,
                    batch_id.as_deref(),
                    input,
                )
                .await?;
            let admission = receiver_control.acquire_target_message_admission_after_binding(
                &receiver_thread, receiver, &sender_thread, sender, &sender_turn_id,
                TargetMessageAdmissionMode::SteerOrWake,
            ).await?;
            let mode = match admission {
                TargetMessageAdmission::Steer => {
                    let _transaction = receiver_control.acquire_response_observation_transaction(receiver).await;
                    let (snapshot, _) = receiver_thread.session.subscribe_agent_responses();
                    let turn_id = snapshot.active_turn_id.ok_or_else(|| CodexErr::InvalidRequest("source turn ended before scoped message admission; input not submitted".into()))?;
                    let rechecked = receiver_control.target_message_admission(
                        receiver, sender, &sender_turn_id, Some(&turn_id),
                        snapshot.last_terminal.as_ref().map(|(turn_id, _)| turn_id.as_str()),
                        TargetMessageAdmissionMode::SteerOrWake,
                    )?;
                    if rechecked != TargetMessageAdmission::Steer {
                        return Err(CodexErr::InvalidRequest("scoped steer authority changed; input not submitted".into()));
                    }
                    codex_protocol::turn_input::TurnInputMode::Steer { expected_turn_id: turn_id }
                }
                TargetMessageAdmission::Wake(_) => codex_protocol::turn_input::TurnInputMode::StartIfIdle,
                TargetMessageAdmission::PendingWake => return Err(CodexErr::InvalidRequest("scoped wake reservation is still pending; this message was not submitted".into())),
            };
            let result = receiver_control.dispatch_user_input_locked(
                &receiver_thread,
                super::user_dispatch::ObservedUserInputRequest {
                    target_message_wake: match admission {
                        TargetMessageAdmission::Wake(reservation_id) => Some(crate::agent::turn_queue::QueuedTargetMessageWake {
                            observer: receiver,
                            target: sender,
                            target_turn_id: sender_turn_id.clone(),
                            reservation_id,
                        }),
                        TargetMessageAdmission::Steer | TargetMessageAdmission::PendingWake => None,
                    },
                    input,
                    start_options,
                    observer: sender,
                    observation: super::user_dispatch::UserObservation::Install(response_observation),
                    dispatch: super::user_dispatch::UserDispatch::Prompt(mode),
                    task_preview: None,
                },
            ).await;
            let (mut submission, input_persisted) = match result {
                Ok(super::user_dispatch::ObservedInputResult::Submitted { submission, input_persisted }) => (submission, input_persisted),
                Ok(super::user_dispatch::ObservedInputResult::PermissionRejected(reason)) => {
                    if let TargetMessageAdmission::Wake(id) = admission {
                        receiver_control.rollback_target_message_wake(receiver, sender, &sender_turn_id, id);
                    }
                    return Err(CodexErr::InvalidRequest(reason));
                }
                Ok(super::user_dispatch::ObservedInputResult::NotSubmitted(reason)) => {
                    if let TargetMessageAdmission::Wake(id) = admission {
                        receiver_control.rollback_target_message_wake(receiver, sender, &sender_turn_id, id);
                    }
                    return Err(CodexErr::InvalidRequest(format!("scoped message was not submitted: {reason:?}")));
                }
                Err(error) => {
                    receiver_control.abandon_response_observer(receiver, sender, &error.to_string());
                    return Err(error);
                }
            };
            if let TargetMessageAdmission::Wake(id) = admission {
                if let Some(turn_id) = submission.target_turn_id.as_deref() {
                    receiver_control.commit_target_message_wake(receiver, sender, &sender_turn_id, id, turn_id);
                    let (snapshot, _) = receiver_thread.session.subscribe_agent_responses();
                    if snapshot.active_turn_id.as_deref() != Some(turn_id)
                        && snapshot.last_terminal.as_ref().is_some_and(|(completed, _)| completed == turn_id)
                    {
                        receiver_control.finish_target_message_wake(receiver, turn_id);
                    }
                    if let Err(error) = receiver_control.persist_response_observation_snapshot(receiver, sender).await {
                        receiver_control.abandon_response_observer(receiver, sender, &error.to_string());
                        submission.post_admission_warning = Some(format!("message was admitted but one-wake publication failed: {error}; do not resend"));
                    }
                } else {
                    receiver_control.abandon_response_observer(receiver, sender, "reverse message admission outcome unknown");
                }
            }
            drop(source_guard);
            drop(lifecycle);
            submission.await_input_persistence(input_persisted).await;
            submission.into_strict_result()
        }).await.map_err(|error| CodexErr::Fatal(format!("scoped input worker lost; reconcile before retry: {error}")))?
    }
}
