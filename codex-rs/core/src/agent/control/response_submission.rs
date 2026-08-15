//! Bind observation to the turn actually admitted by the target, not its submission ID.

use super::response_observer::ResponseObserverStart;
use super::*;
use crate::agent::UserAgentInputOutcome;
use crate::session::ObservedTurnInputSubmission;
use codex_protocol::turn_input::TurnInputMode;

impl LocalAgentControl {
    pub(crate) async fn send_input_observing_response(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<String> {
        let submitted = self.send_input_observing_response_receipt(
            agent_id, input, start_options, observer, policy,
        ).await?;
        match submitted.post_admission_warning {
            Some(warning) => Err(CodexErr::Fatal(warning)),
            None => Ok(submitted.submission_id),
        }
    }

    /// Fresh-spawn cleanup needs to distinguish proven rejection from already enqueued work.
    /// The strict model-tool adapter above retains its error contract; only this receipt can
    /// decide whether disposal of a provisional child is still permitted.
    pub(super) async fn send_input_observing_response_receipt(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<ResponseObservationSubmission> {
        // Once admission starts, caller cancellation must not strand an unbound observation
        // while the target is already executing its input.
        let control = self.clone();
        tokio::spawn(async move {
            control
                .submit_observed_input(agent_id, input, start_options, observer, policy)
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("observed input worker failed: {error}")))?
    }

    pub(super) async fn submit_observed_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<ResponseObservationSubmission> {
        let state = self.runtime.upgrade()?;
        let _lifecycle = state.acquire_live_agent_lifecycle(agent_id).await?;
        let thread = state.get_thread(agent_id).await?;
        thread.ensure_execution_capacity_for_turn_start(self)
            .await?;
        let submission = self.runtime.registry.mailbox_submission(agent_id);
        let _mailbox = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|error| CodexErr::Fatal(format!("mailbox closed: {error}")))?;
        if !self.runtime.registry.submission_is_current(agent_id, &submission)
            || !Arc::ptr_eq(&thread, &state.get_thread(agent_id).await?)
        {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        let observer_thread = state.get_thread(observer.thread_id).await?;
        if observer_thread.session.presentation_id() != observer {
            return Err(CodexErr::ThreadNotFound(observer.thread_id));
        }
        observer_thread.session.submission_admission.check_ready()?;
        thread.session.submission_admission.check_ready()?;
        let _observer_admission = observer_thread
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("observer is closing".to_string()))?;
        let _transaction = self
            .acquire_response_observation_transaction(observer)
            .await;
        let child = thread.session.presentation_id();
        let admission_id = Uuid::now_v7();
        let binding = ResponseObservationBinding::ExplicitAdmission(admission_id);
        self.install_response_observer(
            &observer_thread,
            &thread,
            policy,
            binding,
            ResponseObserverStart::FutureOnly,
        )
        .await?;
        if let Err(error) = self
            .persist_response_observation_snapshot(observer, child)
            .await
        {
            self.abandon_response_observer(observer, child, &error.to_string());
            return Err(error);
        }
        let residency_guard = self.runtime.pin_v2_residency(&state, &thread).await?;
        let last_task_message = non_empty_task_message(render_input_preview(&input));
        let result = thread
            .io
            .submit_observed_turn_input(
                thread.session.as_ref(),
                TurnInputRequest::user_input(input).on_start(start_options),
                TurnInputMode::StartOrSteer,
                residency_guard,
            )
            .await;
        let (submission_id, resolution) = match result {
            Ok(ObservedTurnInputSubmission::Admitted { submission_id, resolution }) => (submission_id, resolution),
            Ok(ObservedTurnInputSubmission::AdmittedWithoutObservation { submission_id, target_turn_id, warning }) => {
                self.runtime.registry.update_last_task_message(agent_id, &submission, last_task_message);
                self.abandon_response_observer(observer, child, &warning);
                return Ok(ResponseObservationSubmission {
                    submission_id, target_turn_id: Some(target_turn_id),
                    input_outcome: UserAgentInputOutcome::Admitted,
                    response_observation: policy, post_admission_warning: Some(warning),
                });
            }
            Ok(ObservedTurnInputSubmission::Indeterminate { submission_id, warning }) => {
                self.abandon_response_observer(observer, child, &warning);
                return Ok(ResponseObservationSubmission {
                    submission_id, target_turn_id: None,
                    input_outcome: UserAgentInputOutcome::Unknown,
                    response_observation: policy, post_admission_warning: Some(warning),
                });
            }
            Ok(ObservedTurnInputSubmission::NotSubmitted { reason }) => {
                self.cancel_response_observation_admission(observer, child, admission_id);
                return Err(CodexErr::InvalidRequest(format!("agent input was not submitted: {reason:?}")));
            }
            Err(error) => {
                self.cancel_response_observation_admission(observer, child, admission_id);
                return self.handle_thread_request_result(agent_id, &state, &thread, Err(error))
                    .await.map(|_| unreachable!("a rejected input cannot produce a submission id"));
            }
        };
        self.runtime.registry
            .update_last_task_message(agent_id, &submission, last_task_message);
        self.bind_response_observation_turn_at_sequence(
            observer,
            child,
            &resolution.target_turn_id,
            binding,
            Some((resolution.minimum_event_sequence, resolution.after_item_id)),
            ResponseObservationBindingPublication::Deferred,
        );
        // Once admitted, failure affects observer delivery, never the accepted target input.
        // Preserve the receipt for spawn cleanup while the strict tool adapter still reports
        // the warning as a non-retryable error to its caller.
        let post_admission_warning = self.persist_response_observation_snapshot(observer, child).await
            .err().map(|error| {
                self.abandon_response_observer(observer, child, &error.to_string());
                format!("target input was already admitted; response observation failed: {error}; do not resend this input")
            });
        if post_admission_warning.is_none() {
            self.publish_response_observation_binding();
        }
        Ok(ResponseObservationSubmission {
            submission_id, target_turn_id: Some(resolution.target_turn_id),
            input_outcome: UserAgentInputOutcome::Admitted,
            response_observation: policy, post_admission_warning,
        })
    }
}
