//! Bind observation to the turn actually admitted by the target, not its submission ID.

use super::response_observer::ResponseObserverStart;
use super::*;
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

    async fn submit_observed_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let _lifecycle = state.acquire_live_agent_lifecycle(agent_id).await?;
        let thread = state.get_thread(agent_id).await?;
        self.ensure_execution_capacity_for_turn_start(&thread)
            .await?;
        let submission = self.state.mailbox_submission(agent_id);
        let _mailbox = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|error| CodexErr::Fatal(format!("mailbox closed: {error}")))?;
        if !self.state.submission_is_current(agent_id, &submission)
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
        let last_task_message = non_empty_task_message(render_input_preview(&input));
        let result = thread
            .io
            .submit_turn_input_with_admission(
                thread.session.as_ref(),
                TurnInputRequest::user_input(input).on_start(start_options),
                TurnInputMode::StartOrSteer,
            )
            .await;
        let (submission_id, resolution) = match result {
            Ok(result) => result,
            Err(error) => {
                self.cancel_response_observation_admission(observer, child, admission_id);
                return self
                    .handle_thread_request_result(agent_id, &state, &thread, Err(error))
                    .await;
            }
        };
        self.state
            .update_last_task_message(agent_id, &submission, last_task_message);
        self.bind_response_observation_turn_at_sequence(
            observer,
            child,
            &resolution.target_turn_id,
            binding,
            Some((resolution.minimum_event_sequence, resolution.after_item_id)),
            ResponseObservationBindingPublication::Deferred,
        );
        // An ambiguous append quarantines this exact observer. Never roll it back by
        // appending a compensating snapshot, nor authorize a same-instance retry.
        if let Err(error) = self
            .persist_response_observation_snapshot(observer, child)
            .await
        {
            self.abandon_response_observer(observer, child, &error.to_string());
            return Err(error);
        }
        self.publish_response_observation_binding();
        Ok(submission_id)
    }
}
