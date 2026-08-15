//! Model-facing adapter to the same typed, cancellation-owned admission boundary.

use super::*;

impl LocalAgentControl {
    pub(crate) async fn send_input_observing_response(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        policy: ResponseObservationPolicy,
    ) -> CodexResult<String> {
        self.send_user_input_observing_response(
            agent_id,
            input,
            start_options,
            observer,
            policy,
            /*task_preview*/ None,
        )
        .await?
        .into_strict_result()
    }
}
