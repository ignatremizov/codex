//! User-only admission. A successful target admission is never reported as a retryable failure.

use super::response_observer::ResponseObserverStart;
use super::*;
use crate::agent::UserAgentInputOutcome;
use crate::session::ObservedTurnInputSubmission;
use codex_protocol::protocol::AgentQueueTurnMetadata;
use codex_protocol::turn_input::NotSubmittedReason;
use codex_protocol::turn_input::TurnInputMode;

#[cfg(test)]
#[path = "user_dispatch_tests.rs"]
mod tests;

pub(super) enum ObservedInputResult {
    Submitted {
        submission: ResponseObservationSubmission,
        input_persisted: Option<tokio::sync::oneshot::Receiver<CodexResult<()>>>,
    },
    NotSubmitted(NotSubmittedReason),
    PermissionRejected(String),
}

impl ObservedInputResult {
    async fn into_user_result(self) -> CodexResult<ResponseObservationSubmission> {
        match self {
            Self::Submitted {
                mut submission,
                input_persisted,
            } => {
                submission.await_input_persistence(input_persisted).await;
                Ok(submission)
            }
            Self::NotSubmitted(reason) => Err(CodexErr::InvalidRequest(format!(
                "agent input was not submitted: {reason:?}"
            ))),
            Self::PermissionRejected(reason) => Err(CodexErr::InvalidRequest(reason)),
        }
    }
}

impl ResponseObservationSubmission {
    pub(super) async fn await_input_persistence(
        &mut self,
        receipt: Option<tokio::sync::oneshot::Receiver<CodexResult<()>>>,
    ) {
        if let Some(receipt) = receipt {
            let error = match receipt.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                let warning = format!(
                    "target input was admitted but durable publication failed: {error}; do not resend"
                );
                self.post_admission_warning = Some(self.post_admission_warning.take().map_or_else(
                    || warning.clone(),
                    |previous| format!("{previous}; {warning}"),
                ));
            }
        }
    }

    pub(super) fn into_strict_result(self) -> CodexResult<String> {
        match self.post_admission_warning {
            Some(warning) => Err(CodexErr::InvalidRequest(warning)),
            None => Ok(self.submission_id),
        }
    }
}

pub(crate) struct ResponseObservationSubmission {
    pub(crate) submission_id: String,
    pub(crate) target_turn_id: Option<String>,
    pub(crate) input_outcome: UserAgentInputOutcome,
    pub(crate) response_observation: ResponseObservationPolicy,
    pub(crate) post_admission_warning: Option<String>,
}

pub(crate) struct ResumeUserInputAdmission {
    pub(crate) input: Vec<UserInput>,
    pub(crate) observer: SessionPresentationId,
    pub(crate) response_observation: ResponseObservationPolicy,
    pub(crate) admission_policy: TurnInputMode,
    pub(crate) task_preview: Option<String>,
}

pub(super) enum UserObservation {
    Install(ResponseObservationPolicy),
    Reserved,
}

pub(super) enum UserDispatch {
    Queued(AgentQueueTurnMetadata),
    Prompt(TurnInputMode),
    InterruptThenPrompt,
}

pub(super) struct ObservedUserInputRequest {
    pub(super) input: AgentControlInput,
    pub(super) start_options: TurnStartOptions,
    pub(super) observer: SessionPresentationId,
    pub(super) observation: UserObservation,
    pub(super) dispatch: UserDispatch,
    pub(super) task_preview: Option<String>,
    pub(super) target_message_wake: Option<crate::agent::turn_queue::QueuedTargetMessageWake>,
}

impl LocalAgentControl {
    pub(crate) async fn send_agent_input_observing_response(
        &self,
        agent_id: ThreadId,
        input: AgentControlInput,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<ResponseObservationSubmission> {
        self.dispatch_user_input(
            agent_id,
            ObservedUserInputRequest {
                target_message_wake: None,
                input,
                start_options,
                observer,
                observation: UserObservation::Install(response_observation),
                dispatch: UserDispatch::Prompt(TurnInputMode::StartOrSteer),
                task_preview: None,
            },
        )
        .await
    }

    pub(super) async fn submit_user_input_to_thread_locked(
        &self,
        thread: &Arc<crate::CodexThread>,
        admission: ResumeUserInputAdmission,
    ) -> CodexResult<ResponseObservationSubmission> {
        self.dispatch_user_input_locked(
            thread,
            ObservedUserInputRequest {
                target_message_wake: None,
                input: AgentControlInput::User(admission.input),
                start_options: TurnStartOptions::default(),
                observer: admission.observer,
                observation: UserObservation::Install(admission.response_observation),
                dispatch: UserDispatch::Prompt(admission.admission_policy),
                task_preview: admission.task_preview,
            },
        )
        .await?
        .into_user_result()
        .await
    }
    pub(crate) async fn send_user_input_observing_response(
        &self,
        agent_id: ThreadId,
        input: Vec<codex_protocol::user_input::UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        response_observation: ResponseObservationPolicy,
        task_preview: Option<String>,
    ) -> CodexResult<ResponseObservationSubmission> {
        self.dispatch_user_input(
            agent_id,
            ObservedUserInputRequest {
                target_message_wake: None,
                input: AgentControlInput::User(input),
                start_options,
                observer,
                observation: UserObservation::Install(response_observation),
                dispatch: UserDispatch::Prompt(TurnInputMode::StartOrSteer),
                task_preview,
            },
        )
        .await
    }

    pub(crate) async fn send_input_using_reserved_response_observation(
        &self,
        agent_id: ThreadId,
        input: Vec<codex_protocol::user_input::UserInput>,
        parent_turn_id: Option<String>,
        observer: SessionPresentationId,
    ) -> CodexResult<ResponseObservationSubmission> {
        let task_preview = Some(render_input_preview(&input));
        self.dispatch_user_input(
            agent_id,
            ObservedUserInputRequest {
                target_message_wake: None,
                input: AgentControlInput::User(input),
                start_options: TurnStartOptions {
                    parent_turn_id,
                    ..Default::default()
                },
                observer,
                observation: UserObservation::Reserved,
                dispatch: UserDispatch::Prompt(TurnInputMode::StartIfIdle),
                task_preview,
            },
        )
        .await
    }

    pub(crate) async fn interrupt_agent_with_user_input_observing_response(
        &self,
        agent_id: ThreadId,
        input: Vec<codex_protocol::user_input::UserInput>,
        observer: SessionPresentationId,
        response_observation: ResponseObservationPolicy,
        task_preview: Option<String>,
    ) -> CodexResult<ResponseObservationSubmission> {
        self.dispatch_user_input(
            agent_id,
            ObservedUserInputRequest {
                target_message_wake: None,
                input: AgentControlInput::User(input),
                start_options: TurnStartOptions::default(),
                observer,
                observation: UserObservation::Install(response_observation),
                dispatch: UserDispatch::InterruptThenPrompt,
                task_preview,
            },
        )
        .await
    }

    async fn dispatch_user_input(
        &self,
        agent_id: ThreadId,
        request: ObservedUserInputRequest,
    ) -> CodexResult<ResponseObservationSubmission> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let _lifecycle = state.acquire_live_agent_lifecycle(agent_id).await?;
            control.require_current_agent_ownership(agent_id).await?;
            let thread = state.get_thread(agent_id).await?;
            let result = control.dispatch_user_input_locked(&thread, request).await?;
            drop(_lifecycle);
            result.into_user_result().await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("user input worker failed: {error}")))?
    }

    pub(super) async fn dispatch_user_input_locked(
        &self,
        thread: &Arc<crate::CodexThread>,
        request: ObservedUserInputRequest,
    ) -> CodexResult<ObservedInputResult> {
        let state = self.upgrade()?;
        let agent_id = thread.session.thread_id();
        let child = thread.session.presentation_id();
        let submission = self.state.mailbox_submission(agent_id);
        let _mailbox = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|error| CodexErr::Fatal(format!("mailbox closed: {error}")))?;
        if !self.state.submission_is_current(agent_id, &submission) {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        thread.session.submission_admission.check_ready()?;
        let observer = state.get_thread(request.observer.thread_id).await?;
        if observer.session.presentation_id() != request.observer {
            return Err(CodexErr::ThreadNotFound(request.observer.thread_id));
        }
        observer.session.submission_admission.check_ready()?;
        let accepted_queue = matches!(&request.dispatch, UserDispatch::Queued(_));
        if let UserObservation::Install(policy) = &request.observation {
            self.ensure_scoped_reply_route_supported(thread, *policy)?;
            if !accepted_queue {
                self.ensure_target_message_route_allowed(thread, request.observer, *policy)?;
            }
        }
        self.ensure_execution_capacity_for_turn_start(thread)
            .await?;
        let mut queue_metadata = None;
        let mode = match request.dispatch {
            UserDispatch::Queued(metadata) => {
                queue_metadata = Some(metadata);
                TurnInputMode::StartIfIdle
            }
            UserDispatch::Prompt(mode) => mode,
            UserDispatch::InterruptThenPrompt => {
                state
                    .send_op_to_thread(
                        thread,
                        Op::Interrupt,
                        /*parent_turn_id*/ None,
                        /*root_turn_id*/ None,
                    )
                    .await?;
                TurnInputMode::StartOrSteer
            }
        };
        let _observer_admission = observer
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".into()))?;
        #[cfg(test)]
        {
            let gate = self
                .wait_agent_presentations
                .scoped_permission_check_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((reached, proceed)) = gate {
                let _ = reached.send(());
                let _ = proceed.await;
            }
        }
        let _permission = self.acquire_messaging_permission_transaction().await;
        if let AgentControlInput::AttributedAgentInput { attribution, .. } = &request.input {
            if attribution.sender.thread_id != request.observer.thread_id
                || attribution.recipient.thread_id != agent_id
            {
                return Err(CodexErr::InvalidRequest(
                    "agent input attribution does not match its endpoints".into(),
                ));
            }
            if let Err(error) = self
                .ensure_model_input_authorized(request.observer, child, &attribution.sender_turn_id)
                .await
            {
                return Ok(ObservedInputResult::PermissionRejected(match error {
                    CodexErr::InvalidRequest(reason) => reason,
                    error => error.to_string(),
                }));
            }
        }
        if let Some(wake) = &request.target_message_wake
            && !self.target_message_wake_is_current(wake)
        {
            return Ok(ObservedInputResult::PermissionRejected(
                "scoped wake permission ended before target-turn admission; input not submitted"
                    .into(),
            ));
        }
        let transaction = self
            .acquire_response_observation_transaction(request.observer)
            .await;
        let (policy, binding) = match request.observation {
            UserObservation::Install(policy) => {
                let binding = ResponseObservationBinding::ExplicitAdmission(Uuid::now_v7());
                self.install_response_observer(
                    &observer,
                    thread,
                    policy,
                    binding,
                    ResponseObserverStart::FutureOnly,
                )
                .await?;
                if let Err(error) = self
                    .persist_response_observation_snapshot(request.observer, child)
                    .await
                {
                    self.abandon_response_observer(request.observer, child, &error.to_string());
                    return Err(error);
                }
                (policy, binding)
            }
            UserObservation::Reserved => {
                let policy = self
                    .reserved_response_observation_policy(request.observer, child)
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest("target has no reserved response policy".into())
                    })?;
                (policy, ResponseObservationBinding::NextTurn)
            }
        };
        let last_task_message =
            non_empty_task_message(render_input_preview(request.input.presentation()));
        // The forward prompt was already accepted. Preserve its policy/receipt without publishing
        // new reverse authority or misleading route guidance after an explicit user disable.
        let input = if accepted_queue
            && self.target_message_route_mode(request.observer, child)
                == Some(TargetMessageRouteMode::Disabled)
        {
            request.input
        } else {
            self.with_agent_reply_route(thread, request.observer, policy, request.input)
                .await?
        };
        let input = input.into_request().on_start(request.start_options);
        #[cfg(test)]
        if matches!(&mode, TurnInputMode::Steer { .. }) {
            let gate = self
                .wait_agent_presentations
                .scoped_steer_submission_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((reached, proceed)) = gate {
                let _ = reached.send(());
                let _ = proceed.await;
            }
        }
        let admitted = match queue_metadata {
            Some(metadata) => {
                thread
                    .io
                    .submit_observed_queued_turn_input(thread.session.as_ref(), input, metadata)
                    .await
            }
            None => {
                thread
                    .io
                    .submit_observed_turn_input(thread.session.as_ref(), input, mode)
                    .await
            }
        };
        let (submission_id, resolution, queue_start_permit, input_persisted) = match admitted {
            Ok(ObservedTurnInputSubmission::Admitted {
                submission_id,
                resolution,
                queue_start_permit,
                input_persisted,
            }) => (
                submission_id,
                resolution,
                queue_start_permit,
                input_persisted,
            ),
            Ok(ObservedTurnInputSubmission::AdmittedWithoutObservation {
                submission_id,
                target_turn_id,
                warning,
            }) => {
                self.abandon_response_observer(request.observer, child, &warning);
                return Ok(ObservedInputResult::Submitted {
                    input_persisted: None,
                    submission: ResponseObservationSubmission {
                        submission_id,
                        target_turn_id: Some(target_turn_id),
                        response_observation: policy,
                        input_outcome: UserAgentInputOutcome::Admitted,
                        post_admission_warning: Some(format!(
                            "{warning}; input was admitted; do not resend"
                        )),
                    },
                });
            }
            Ok(ObservedTurnInputSubmission::Indeterminate {
                submission_id,
                warning,
            }) => {
                self.abandon_response_observer(request.observer, child, &warning);
                return Ok(ObservedInputResult::Submitted {
                    input_persisted: None,
                    submission: ResponseObservationSubmission {
                        submission_id,
                        target_turn_id: None,
                        response_observation: policy,
                        input_outcome: UserAgentInputOutcome::Unknown,
                        post_admission_warning: Some(format!(
                            "{warning}; input outcome unknown; reload and reconcile before retry"
                        )),
                    },
                });
            }
            Ok(ObservedTurnInputSubmission::NotSubmitted { reason }) => {
                if let ResponseObservationBinding::ExplicitAdmission(id) = binding {
                    self.cancel_response_observation_admission(request.observer, child, id);
                }
                return Ok(ObservedInputResult::NotSubmitted(reason));
            }
            Err(error) => {
                if let ResponseObservationBinding::ExplicitAdmission(id) = binding {
                    self.cancel_response_observation_admission(request.observer, child, id);
                }
                return Err(error);
            }
        };
        self.state
            .update_last_task_message(agent_id, &submission, last_task_message);
        self.bind_response_observation_turn_at_sequence(
            request.observer,
            child,
            &resolution.target_turn_id,
            binding,
            Some((resolution.minimum_event_sequence, resolution.after_item_id)),
            ResponseObservationBindingPublication::Deferred,
        );
        let result = self
            .publish_user_task_observation(
                &observer,
                child,
                Some(resolution.target_turn_id.clone()),
                policy,
                request.task_preview,
                transaction,
            )
            .await;
        let post_admission_warning = result.err().map(|error| {
            self.abandon_response_observer(request.observer, child, &error.to_string());
            format!("target input was already admitted; response observation failed: {error}; do not resend this input")
        });
        self.publish_response_observation_binding();
        if let Some(permit) = queue_start_permit {
            if post_admission_warning.is_none() {
                permit.publish();
            } else {
                permit.publish_without_response_handling();
            }
        }
        Ok(ObservedInputResult::Submitted {
            input_persisted,
            submission: ResponseObservationSubmission {
                submission_id,
                target_turn_id: Some(resolution.target_turn_id),
                input_outcome: UserAgentInputOutcome::Admitted,
                response_observation: policy,
                post_admission_warning,
            },
        })
    }
}
