use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use std::sync::Arc;

use super::UserAgentFinalResponseHandling;
use super::UserAgentObservationBinding;
use super::UserAgentObservationMode;
use super::UserAgentOwnershipTransfer;
use super::UserAgentPromptResult;
use super::UserAgentReplyRouteMode;
use super::UserAgentResponseHandling;
use super::UserAgentResumeResult;
use super::child_session_source;
use super::control_relationship_source;
use crate::CodexThread;
use crate::agent::AgentStatus;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::control::AgentResumeOwnership;
use crate::agent::control::ResumeUserInputAdmission;
use crate::agent::control::TargetMessageRouteMode;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::config::Config;

struct PreparedClosedAgentResume {
    config: Config,
    session_source: SessionSource,
}

impl CodexThread {
    /// Resume a controlled closed agent or explicitly adopt a stored agent by UUID.
    ///
    /// A live runtime owned by another root is never adopted in place because its session still
    /// carries the old controller. The caller must first close that runtime, after which this
    /// operation can transfer its durable alias and reopen it exclusively.
    pub async fn resume_agent(
        &self,
        target: &str,
        response_handling: UserAgentResponseHandling,
    ) -> CodexResult<UserAgentResumeResult> {
        self.resume_agent_inner(target, response_handling.into())
            .await
    }

    /// Replace final-response handling for the target's active, pending, or undelivered turn.
    pub async fn observe_agent(
        &self,
        target: &str,
        response_handling: UserAgentObservationMode,
    ) -> CodexResult<(
        ThreadId,
        UserAgentFinalResponseHandling,
        UserAgentObservationBinding,
    )> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot observe itself".to_string(),
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
        let replacement = match response_handling {
            UserAgentObservationMode::Passive => FinalResponseObservation::Passive,
            UserAgentObservationMode::Wake => FinalResponseObservation::Wake,
            UserAgentObservationMode::Presentation => FinalResponseObservation::PresentationOnly,
        };
        let replaced = agent_control
            .replace_durable_final_response_observation(
                target_thread_id,
                self.session.presentation_id(),
                replacement,
            )
            .await?;
        Ok((
            replaced.target_thread_id,
            replaced.previous.into(),
            replaced.binding.into(),
        ))
    }

    /// Enable or disable the target's attributed reply route back to this source.
    pub async fn set_agent_reply_route(
        &self,
        target: &str,
        mode: UserAgentReplyRouteMode,
    ) -> CodexResult<(ThreadId, Option<UserAgentReplyRouteMode>)> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot grant itself a reply route".to_string(),
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
        let replacement = match mode {
            UserAgentReplyRouteMode::Enabled => TargetMessageRouteMode::Enabled,
            UserAgentReplyRouteMode::Disabled => TargetMessageRouteMode::Disabled,
        };
        let replaced = agent_control
            .replace_durable_target_message_route(
                target_thread_id,
                self.session.presentation_id(),
                replacement,
            )
            .await?;
        let previous = replaced.previous.map(|mode| match mode {
            TargetMessageRouteMode::Enabled => UserAgentReplyRouteMode::Enabled,
            TargetMessageRouteMode::Disabled => UserAgentReplyRouteMode::Disabled,
        });
        Ok((replaced.target_thread_id, previous))
    }

    pub(super) async fn resume_agent_inner(
        &self,
        target: &str,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<UserAgentResumeResult> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control.resolve_resumable_agent_target(target).await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot resume itself".to_string(),
            ));
        }

        let resume_plan = agent_control.plan_agent_resume(target_thread_id).await?;
        let observed_status = resume_plan.status;
        let target_is_live = !matches!(observed_status, AgentStatus::NotFound);
        if target_is_live {
            let turn = self.session.new_default_turn().await;
            let session_source = control_relationship_source(
                self,
                turn.as_ref(),
                /*role*/ None,
                /*task_name*/ None,
            )?;
            let task_preview = matches!(observed_status, AgentStatus::Running)
                .then(|| {
                    agent_control
                        .get_agent_metadata(target_thread_id)
                        .and_then(|metadata| metadata.last_task_message)
                })
                .flatten();
            let status = agent_control
                .ensure_durable_completion_watcher(
                    target_thread_id,
                    session_source,
                    response_observation,
                    observed_status,
                    task_preview,
                )
                .await?;
            let observation_binding = agent_control
                .current_response_observation_binding_for_thread(
                    self.session.presentation_id(),
                    target_thread_id,
                )
                .await
                .map(Into::into);
            let (agent_ref, nickname) = resume_plan.current_alias.map_or((None, None), |alias| {
                (Some(alias.agent_ref), alias.nickname)
            });
            return Ok(UserAgentResumeResult {
                target_thread_id,
                agent_ref,
                nickname,
                status,
                ownership_transfer: None,
                observation_binding,
                post_commit_warning: None,
            });
        }

        let prepared = self
            .prepare_closed_agent_resume(resume_plan.ownership)
            .await?;
        let outcome = match resume_plan.ownership {
            AgentResumeOwnership::CurrentRoot => {
                agent_control
                    .resume_user_agent_from_rollout(
                        prepared.config,
                        target_thread_id,
                        prepared.session_source,
                        response_observation,
                    )
                    .await?
            }
            AgentResumeOwnership::Transfer {
                previous_session_id,
            } => {
                agent_control
                    .resume_user_agent_from_rollout_adopting(
                        prepared.config,
                        target_thread_id,
                        prepared.session_source,
                        response_observation,
                        previous_session_id,
                        target.to_string(),
                    )
                    .await?
            }
        };
        let status = agent_control.get_status(target_thread_id).await;
        let observation_binding = agent_control
            .current_response_observation_binding_for_thread(
                self.session.presentation_id(),
                target_thread_id,
            )
            .await
            .map(Into::into);
        let (agent_ref, nickname) = outcome.alias.map_or((None, None), |alias| {
            (Some(alias.agent_ref), alias.nickname)
        });
        Ok(UserAgentResumeResult {
            target_thread_id,
            agent_ref,
            nickname,
            status,
            ownership_transfer: match outcome.transfer {
                Some(codex_agent_graph_store::AgentAliasTransfer::Transferred {
                    previous_session_id,
                    alias,
                    ..
                }) => Some(UserAgentOwnershipTransfer {
                    previous_session_id,
                    new_session_id: alias.session_id,
                }),
                Some(codex_agent_graph_store::AgentAliasTransfer::AlreadyOwned { .. }) | None => {
                    None
                }
            },
            observation_binding,
            post_commit_warning: outcome.post_commit_warning,
        })
    }

    async fn prepare_closed_agent_resume(
        &self,
        ownership: AgentResumeOwnership,
    ) -> CodexResult<PreparedClosedAgentResume> {
        let agent_control = &self.session.services.agent_control;
        let turn = self.session.new_default_turn().await;
        let ownership_transfer = match ownership {
            AgentResumeOwnership::CurrentRoot => None,
            AgentResumeOwnership::Transfer {
                previous_session_id,
            } => Some(UserAgentOwnershipTransfer {
                previous_session_id,
                new_session_id: agent_control.session_id(),
            }),
        };
        let task_name = ownership_transfer
            .is_some()
            .then(|| format!("user-adopt-{}", uuid::Uuid::now_v7()));
        let session_source = if ownership_transfer.is_some() {
            child_session_source(self, turn.as_ref(), /*role*/ None, task_name)?
        } else {
            // Same-root resume restores the target's persisted parent and depth. The current
            // source is only the response observer and may itself be at the spawn-depth limit.
            control_relationship_source(
                self,
                turn.as_ref(),
                /*role*/ None,
                /*task_name*/ None,
            )?
        };
        let config = build_agent_resume_config(turn.as_ref()).map_err(CodexErr::InvalidRequest)?;
        Ok(PreparedClosedAgentResume {
            config,
            session_source,
        })
    }

    pub(super) async fn resume_closed_agent_with_input(
        &self,
        target_thread_id: ThreadId,
        authored_selector: String,
        input: Vec<UserInput>,
        response_handling: UserAgentResponseHandling,
    ) -> CodexResult<UserAgentPromptResult> {
        let agent_control = &self.session.services.agent_control;
        let resume_plan = agent_control.plan_agent_resume(target_thread_id).await?;
        if resume_plan.ownership.transfers_ownership() {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent {target_thread_id} is no longer controlled by this root; use resume_agent \
                 to adopt it"
            )));
        }
        let prepared = self
            .prepare_closed_agent_resume(AgentResumeOwnership::CurrentRoot)
            .await?;
        let task_preview = response_handling
            .exposes_task_context()
            .then(|| crate::agent::control::render_input_preview(&input));
        if response_handling.queue_input() {
            let resumed = agent_control
                .resume_user_agent_from_rollout(
                    prepared.config,
                    target_thread_id,
                    prepared.session_source,
                    ResponseObservationPolicy::from_parts(
                        /*commentary*/ false,
                        FinalResponseObservation::None,
                    ),
                )
                .await?;
            let response_observation = ResponseObservationPolicy::from(response_handling);
            let submission = agent_control
                .queue_input_observing_response(
                    crate::agent::control::QueuedInputObservationParams {
                        agent_id: target_thread_id,
                        input,
                        start_options: TurnStartOptions::default(),
                        observer: self.session.presentation_id(),
                        response_observation,
                        task_preview,
                        authored_selector: Some(authored_selector),
                    },
                )
                .await?;
            return Ok(UserAgentPromptResult {
                target_thread_id,
                submission_id: submission.queue_id.to_string(),
                queued: true,
                input_outcome: super::UserAgentInputOutcome::Queued,
                resumed_target: true,
                post_admission_warning: resumed.post_commit_warning,
            });
        }
        let response_observation = ResponseObservationPolicy::from(response_handling);
        let admission = ResumeUserInputAdmission {
            input,
            observer: self.session.presentation_id(),
            response_observation,
            admission_policy: TurnInputMode::StartOrSteer,
            task_preview,
        };
        let submission = agent_control
            .resume_agent_from_rollout_with_user_input(
                prepared.config,
                target_thread_id,
                prepared.session_source,
                admission,
            )
            .await?;
        Ok(UserAgentPromptResult {
            target_thread_id,
            submission_id: submission.submission_id,
            queued: false,
            input_outcome: submission.input_outcome,
            resumed_target: true,
            post_admission_warning: submission.post_admission_warning,
        })
    }

    /// Interrupt a controlled live agent and optionally admit a follow-up as its next input.
    pub async fn interrupt_agent(
        &self,
        target: &str,
        follow_up: Option<(Vec<UserInput>, UserAgentResponseHandling)>,
    ) -> CodexResult<(ThreadId, Option<UserAgentPromptResult>)> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "interrupt the current agent through the normal shortcut".to_string(),
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
        if follow_up
            .as_ref()
            .is_some_and(|(input, _response_handling)| input.is_empty())
        {
            return Err(CodexErr::InvalidRequest(
                "agent follow-up requires nonempty user input".to_string(),
            ));
        }

        let Some((input, response_handling)) = follow_up else {
            agent_control.interrupt_agent(target_thread_id).await?;
            return Ok((target_thread_id, None));
        };
        let task_preview = Some(crate::agent::control::render_input_preview(&input));
        let submission = agent_control
            .interrupt_agent_with_user_input_observing_response(
                target_thread_id,
                input,
                self.session.presentation_id(),
                response_handling.into(),
                task_preview,
            )
            .await?;
        Ok((
            target_thread_id,
            Some(UserAgentPromptResult {
                target_thread_id,
                submission_id: submission.submission_id,
                queued: false,
                input_outcome: submission.input_outcome,
                resumed_target: false,
                post_admission_warning: submission.post_admission_warning,
            }),
        ))
    }

    /// Explicitly close a controlled agent runtime.
    pub async fn close_agent(
        &self,
        target: &str,
        response_handling: UserAgentResponseHandling,
    ) -> CodexResult<ThreadId> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot close itself".to_string(),
            ));
        }
        if target_thread_id == ThreadId::from(agent_control.session_id()) {
            return Err(CodexErr::InvalidRequest(
                "a child agent cannot close Main".to_string(),
            ));
        }
        let close_response = agent_control
            .prepare_close_agent_response(
                Arc::clone(&self.session),
                self.multi_agent_version()
                    .unwrap_or(codex_protocol::protocol::MultiAgentVersion::V1),
                target_thread_id,
            )
            .await?;
        let closed = agent_control
            .close_agent_with_status(target_thread_id)
            .await?;
        let response_warning = match close_response
            .deliver(&closed, response_handling.into())
            .await
        {
            Ok(crate::agent::control::CloseAgentResponseDisposition::DeliveryPending) => Some(
                "the earlier exact-session delivery still owns its receipt; delivery is pending, not acknowledged".to_string()
            ),
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        };
        if let Some(warning) = response_warning {
            tracing::warn!(%target_thread_id, "agent closed; response pending or failed: {warning}");
            self.session.send_event_raw(codex_protocol::protocol::Event {
                id: format!("agent-close-{target_thread_id}"),
                msg: codex_protocol::protocol::EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                    message: format!("Agent {target_thread_id} was closed, but its response could not be acknowledged: {warning}. Do not replay automatically."),
                }),
            }).await;
        }
        Ok(target_thread_id)
    }
}
