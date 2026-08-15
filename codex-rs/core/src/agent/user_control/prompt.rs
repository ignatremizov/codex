use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::user_input::UserInput;

use super::UserAgentPromptResult;
use super::UserAgentReservedPromptResult;
use super::UserAgentResponseHandling;
use crate::CodexThread;
use crate::agent::AgentStatus;
use crate::session::InputTurnAdmissionPolicy;

impl CodexThread {
    /// Admit genuine user input to a live agent controlled by this thread's root.
    ///
    /// Target resolution and response observation are source-relative. A known same-root closed
    /// target is reopened before admission; out-of-root UUIDs still require explicit adoption.
    pub async fn prompt_live_agent(
        &self,
        target: &str,
        input: Vec<UserInput>,
        response_handling: UserAgentResponseHandling,
    ) -> CodexResult<UserAgentPromptResult> {
        self.prompt_agent(
            target,
            input,
            response_handling,
            InputTurnAdmissionPolicy::AnyTurn,
        )
        .await
    }

    /// Admit genuine user input only if the controlled target is still idle.
    ///
    /// This keeps process-local queued follow-ups from steering a newer turn that raced their
    /// liveness check. The caller retains the queued item and retries after that turn completes.
    pub async fn prompt_idle_agent(
        &self,
        target: &str,
        input: Vec<UserInput>,
        response_handling: UserAgentResponseHandling,
    ) -> CodexResult<UserAgentPromptResult> {
        self.prompt_agent(
            target,
            input,
            response_handling,
            InputTurnAdmissionPolicy::IdleOnly,
        )
        .await
    }

    async fn prompt_agent(
        &self,
        target: &str,
        input: Vec<UserInput>,
        response_handling: UserAgentResponseHandling,
        admission: InputTurnAdmissionPolicy,
    ) -> CodexResult<UserAgentPromptResult> {
        if input.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "agent prompt requires nonempty user input".to_string(),
            ));
        }

        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "prompt the current agent through the normal composer".to_string(),
            ));
        }
        let resumed_target = matches!(
            agent_control.get_status(target_thread_id).await,
            AgentStatus::NotFound
        );
        if resumed_target {
            return self
                .resume_closed_agent_with_input(
                    target_thread_id,
                    input,
                    response_handling,
                    admission,
                )
                .await;
        }

        let task_preview = response_handling
            .exposes_task_context()
            .then(|| crate::agent::control::render_input_preview(&input));
        let submission = match admission {
            InputTurnAdmissionPolicy::AnyTurn => {
                agent_control
                    .send_user_input_observing_response(
                        target_thread_id,
                        input,
                        /*parent_turn_id*/ None,
                        self.session.presentation_id(),
                        response_handling.into(),
                        task_preview,
                    )
                    .await?
            }
            InputTurnAdmissionPolicy::IdleOnly => {
                agent_control
                    .send_idle_user_input_observing_response(
                        target_thread_id,
                        input,
                        /*parent_turn_id*/ None,
                        self.session.presentation_id(),
                        response_handling.into(),
                        task_preview,
                    )
                    .await?
            }
        };
        Ok(UserAgentPromptResult {
            target_thread_id,
            submission_id: submission.submission_id,
            resumed_target,
            post_admission_warning: submission.post_admission_warning,
        })
    }

    /// Admit input to an idle target using its reserved next-turn response policy.
    ///
    /// A prompt-less spawn or resume already reserved response handling for this turn. This
    /// operation consumes that reservation instead of installing a second policy.
    pub async fn prompt_live_agent_using_reserved_response(
        &self,
        target: &str,
        input: Vec<UserInput>,
    ) -> CodexResult<UserAgentReservedPromptResult> {
        if input.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "agent prompt requires nonempty user input".to_string(),
            ));
        }

        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "prompt the current agent through the normal composer".to_string(),
            ));
        }
        if matches!(
            agent_control.get_status(target_thread_id).await,
            AgentStatus::NotFound
        ) {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {target_thread_id} is closed"
            )));
        }

        let submission = agent_control
            .send_input_using_reserved_response_observation(
                target_thread_id,
                input,
                /*parent_turn_id*/ None,
                self.session.presentation_id(),
            )
            .await?;
        Ok(UserAgentReservedPromptResult {
            target_thread_id,
            submission_id: submission.submission_id,
            target_turn_id: submission.target_turn_id,
            response_handling: UserAgentResponseHandling::from_parts(
                submission.response_observation.commentary(),
                submission.response_observation.final_response().into(),
                submission.response_observation.target_messages(),
                /*queue_input*/ false,
            ),
            post_admission_warning: submission.post_admission_warning,
        })
    }
}
