use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
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
use super::user_control_tool_error;
use crate::CodexThread;
use crate::agent::AgentStatus;
use crate::agent::control::AgentResumeOwnership;
use crate::agent::control::InputTurnAdmissionMode;
use crate::agent::control::ResumeUserInputAdmission;
use crate::agent::control::TargetMessageRouteMode;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::config::Config;
use crate::tools::handlers::multi_agents_common::build_agent_resume_config;

struct PreparedClosedAgentResume {
    config: Config,
    session_source: SessionSource,
    ownership_transfer: Option<UserAgentOwnershipTransfer>,
}

impl CodexThread {
    /// Persist a messaging default for this supervisor and its current/future descendants.
    /// The setting survives restart independently of response subscriptions.
    pub async fn set_agent_subtree_messaging(
        &self,
        mode: UserAgentReplyRouteMode,
    ) -> CodexResult<Option<UserAgentReplyRouteMode>> {
        let mode = match mode {
            UserAgentReplyRouteMode::Enabled => TargetMessageRouteMode::Enabled,
            UserAgentReplyRouteMode::Disabled => TargetMessageRouteMode::Disabled,
        };
        self.session
            .services
            .agent_control
            .set_subtree_messaging(self.session.presentation_id(), mode)
            .await
            .map(|previous| {
                previous.map(|mode| match mode {
                    TargetMessageRouteMode::Enabled => UserAgentReplyRouteMode::Enabled,
                    TargetMessageRouteMode::Disabled => UserAgentReplyRouteMode::Disabled,
                })
            })
    }

    /// Resume a controlled closed agent or explicitly adopt a stored agent by UUID.
    ///
    /// `task` assigns a caller-relative label only during cross-root adoption. Same-root resume
    /// preserves the existing assignment and rejects a task override.
    ///
    /// A live runtime owned by another root is never adopted in place because its session still
    /// carries the old controller. The caller must first close that runtime, after which this
    /// operation can transfer its durable alias and reopen it exclusively.
    pub async fn resume_agent(
        &self,
        target: &str,
        task: Option<String>,
        response_handling: UserAgentResponseHandling,
    ) -> CodexResult<UserAgentResumeResult> {
        self.resume_agent_inner(target, task, response_handling.into())
            .await
    }

    /// Replace an existing observer-to-target final-response subscription.
    /// Omitted observer means the issuing thread; both selectors stay within its control graph.
    pub async fn observe_agent(
        &self,
        target: &str,
        observer: Option<&str>,
        response_handling: UserAgentObservationMode,
    ) -> CodexResult<(
        ThreadId,
        ThreadId,
        UserAgentFinalResponseHandling,
        UserAgentObservationBinding,
    )> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(self.session.thread_id(), target)
            .await?;
        let observer_thread_id = match observer {
            Some(observer) => {
                agent_control
                    .resolve_controlled_agent_target(source_thread_id, observer)
                    .await?
            }
            None => source_thread_id,
        };
        if target_thread_id == observer_thread_id {
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
                observer_thread_id,
                replacement,
            )
            .await?;
        Ok((
            replaced.target_thread_id,
            observer_thread_id,
            replaced.previous.into(),
            replaced.binding.into(),
        ))
    }

    /// Enable or disable a sender's attributed reply route within this control graph.
    /// Omitted recipient means the thread issuing the user command.
    pub async fn set_agent_reply_route(
        &self,
        target: &str,
        recipient: Option<&str>,
        mode: UserAgentReplyRouteMode,
    ) -> CodexResult<(ThreadId, ThreadId, Option<UserAgentReplyRouteMode>)> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_controlled_agent_target(self.session.thread_id(), target)
            .await?;
        let recipient_thread_id = match recipient {
            Some(recipient) => {
                agent_control
                    .resolve_controlled_agent_target(self.session.thread_id(), recipient)
                    .await?
            }
            None => source_thread_id,
        };
        if target_thread_id == recipient_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot grant itself a reply route".to_string(),
            ));
        }
        if mode == UserAgentReplyRouteMode::Disabled
            && agent_control
                .is_live_agent_descendant(target_thread_id, recipient_thread_id)
                .await?
        {
            return Err(CodexErr::InvalidRequest(
                "cannot disable supervisor send_input to a descendant; task dispatch remains enabled"
                    .to_string(),
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
                recipient_thread_id,
                replacement,
            )
            .await?;
        let previous = replaced.previous.map(|mode| match mode {
            TargetMessageRouteMode::Enabled => UserAgentReplyRouteMode::Enabled,
            TargetMessageRouteMode::Disabled => UserAgentReplyRouteMode::Disabled,
        });
        Ok((replaced.target_thread_id, recipient_thread_id, previous))
    }

    pub(super) async fn resume_agent_inner(
        &self,
        target: &str,
        task: Option<String>,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<UserAgentResumeResult> {
        let source_thread_id = self.session.thread_id();
        let agent_control = &self.session.services.agent_control;
        let target_thread_id = agent_control
            .resolve_resumable_agent_target(source_thread_id, target)
            .await?;
        if target_thread_id == source_thread_id {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot resume itself".to_string(),
            ));
        }

        let resume_plan = agent_control.plan_agent_resume(target_thread_id).await?;
        if task.is_some() && !resume_plan.ownership.transfers_ownership() {
            return Err(CodexErr::InvalidRequest(
                "task is only supported when adopting an agent from another root; \
                 same-root resume does not rename its assignment"
                    .to_string(),
            ));
        }
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
            let (agent_ref, nickname, task_path) = resume_plan
                .current_alias
                .map_or((None, None, None), |alias| {
                    (Some(alias.agent_ref), alias.nickname, alias.task_path)
                });
            return Ok(UserAgentResumeResult {
                target_thread_id,
                agent_ref,
                nickname,
                task_path,
                status,
                ownership_transfer: None,
                observation_binding,
                post_commit_warning: None,
            });
        }

        let mut prepared = self
            .prepare_closed_agent_resume(resume_plan.ownership)
            .await?;
        let mut post_commit_warning = None;
        let persisted_alias = match resume_plan.ownership {
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
                let adoption = agent_control
                    .resume_user_agent_from_rollout_adopting(
                        prepared.config,
                        target_thread_id,
                        prepared.session_source,
                        response_observation,
                        previous_session_id,
                        target.to_string(),
                        task,
                    )
                    .await;
                match adoption {
                    Ok(persisted_alias) => persisted_alias.map(|persisted| {
                        if let Some(transfer) = prepared.ownership_transfer.as_mut() {
                            transfer.task_path_mapping = persisted.task_path_mapping;
                        }
                        persisted.alias
                    }),
                    Err(err) => {
                        let alias_after_failure = match agent_control
                            .current_agent_alias(target_thread_id)
                            .await
                        {
                            Ok(alias) => alias,
                            Err(owner_error) => {
                                return Err(CodexErr::Fatal(format!(
                                    "{err}; adoption ownership could not be verified after resume setup \
                             failed: {owner_error}"
                                )));
                            }
                        };
                        if alias_after_failure.as_ref().map(|alias| alias.session_id)
                            != Some(agent_control.session_id())
                        {
                            return Err(err);
                        }
                        post_commit_warning = Some(format!(
                            "agent is now owned by this root, but resume setup did not finish: {err}; \
                         retry `/agent resume {target_thread_id}` to reopen it"
                        ));
                        alias_after_failure
                    }
                }
            }
        };
        let status = agent_control.get_status(target_thread_id).await;
        let observation_binding = if post_commit_warning.is_some() {
            None
        } else {
            agent_control
                .current_response_observation_binding_for_thread(
                    self.session.presentation_id(),
                    target_thread_id,
                )
                .await
                .map(Into::into)
        };
        let (agent_ref, nickname, task_path) = persisted_alias
            .map_or((None, None, None), |alias| {
                (Some(alias.agent_ref), alias.nickname, alias.task_path)
            });
        Ok(UserAgentResumeResult {
            target_thread_id,
            agent_ref,
            nickname,
            task_path,
            status,
            ownership_transfer: prepared.ownership_transfer,
            observation_binding,
            post_commit_warning,
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
                task_path_mapping: Vec::new(),
            }),
        };
        let task_name = ownership_transfer
            .is_some()
            .then(|| format!("user_adopt_{}", uuid::Uuid::now_v7().as_simple()));
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
        let config = build_agent_resume_config(turn.as_ref()).map_err(user_control_tool_error)?;
        Ok(PreparedClosedAgentResume {
            config,
            session_source,
            ownership_transfer,
        })
    }

    pub(super) async fn resume_closed_agent_with_input(
        &self,
        target_thread_id: ThreadId,
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
        let response_observation = ResponseObservationPolicy::from(response_handling);
        let queue_id = response_handling.queue_input().then(uuid::Uuid::now_v7);
        let admission_mode =
            queue_id
                .as_ref()
                .map_or(InputTurnAdmissionMode::AnyTurn, |queue_id| {
                    InputTurnAdmissionMode::Queued(
                        response_observation.admitted_queue_turn_metadata(
                            queue_id.to_string(),
                            self.session.thread_id(),
                        ),
                    )
                });
        let admission = ResumeUserInputAdmission {
            input,
            observer: self.session.presentation_id(),
            response_observation,
            admission_mode,
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
        let submission_id = match queue_id {
            Some(queue_id) => queue_id.to_string(),
            None => submission.submission_id,
        };
        Ok(UserAgentPromptResult {
            target_thread_id,
            submission_id,
            queued: response_handling.queue_input(),
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
            .resolve_controlled_agent_target(self.session.thread_id(), target)
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
        let task_preview = response_handling
            .exposes_task_context()
            .then(|| crate::agent::control::render_input_preview(&input));
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
            .resolve_controlled_agent_target(self.session.thread_id(), target)
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
        close_response
            .deliver(&closed.previous_status, response_handling.into())
            .await;
        Ok(target_thread_id)
    }
}
