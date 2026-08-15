//! Typed user-authored agent lifecycle requests and source-side audit persistence.

use super::*;
use conversion::agent_control_error;
use conversion::agent_final_response_handling;
use conversion::agent_observation_binding;
use conversion::observation_mode_final_response_handling;
use conversion::user_agent_control_item;
use conversion::user_agent_final_response_handling;
use conversion::user_agent_response_handling;

mod conversion;

impl ThreadRequestProcessor {
    pub(super) async fn agent_control_response_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: AgentControlParams,
    ) -> Result<AgentControlResponse, JSONRPCErrorError> {
        let AgentControlParams {
            source_thread_id,
            authored_selector,
            action,
        } = params;
        let source_thread_id = ThreadId::from_string(&source_thread_id)
            .map_err(|err| invalid_request(format!("invalid source thread id: {err}")))?;
        let source_thread = self
            .thread_manager
            .get_thread(source_thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {source_thread_id}")))?;
        let mut audit_item = user_agent_control_item(&action, authored_selector.as_deref());
        if let Some(target) = agent_control_action_target(&action) {
            audit_item.target_thread_id =
                source_thread.resolve_user_agent_target(target).await.ok();
        }
        let operation = async {
            match action {
                AgentControlAction::Spawn {
                    role,
                    input,
                    fork_mode,
                    response_handling,
                } => {
                    let input = input
                        .map(|input| {
                            validate_user_input_image_urls(&input)?;
                            validate_v2_input_limit(&input)?;
                            Ok(input.into_iter().map(V2UserInput::into_core).collect())
                        })
                        .transpose()?;
                    let fork_mode = match fork_mode {
                        AgentForkMode::None => UserAgentForkMode::None,
                        AgentForkMode::All => UserAgentForkMode::All,
                        AgentForkMode::LastNTurns { turns } => {
                            let turns = usize::try_from(turns).map_err(|_| {
                                invalid_request("agent fork turn count exceeds platform limits")
                            })?;
                            if turns == 0 {
                                return Err(invalid_request(
                                    "last-N agent forks require a positive turn count",
                                ));
                            }
                            UserAgentForkMode::LastNTurns(turns)
                        }
                    };
                    let response_handling = response_handling
                        .map(user_agent_response_handling)
                        .unwrap_or_default();
                    let result = source_thread
                        .spawn_agent(role.as_deref(), input, fork_mode, response_handling)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        result.target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(result.target_thread_id);
                    audit_item.agent_ref = result.agent_ref;
                    audit_item.nickname = result.nickname.clone();
                    audit_item.error = result.post_admission_warning.clone();
                    Ok(AgentControlOutcome::Spawned {
                        target_thread_id: result.target_thread_id.to_string(),
                        agent_ref: result.agent_ref.map(|agent_ref| agent_ref.to_string()),
                        nickname: result.nickname,
                        post_admission_warning: result.post_admission_warning,
                    })
                }
                AgentControlAction::Prompt {
                    target,
                    input,
                    response_handling,
                } => {
                    validate_user_input_image_urls(&input)?;
                    validate_v2_input_limit(&input)?;
                    let input = input.into_iter().map(V2UserInput::into_core).collect();
                    let response_handling = response_handling
                        .map(user_agent_response_handling)
                        .unwrap_or_default();
                    let result = source_thread
                        .prompt_live_agent(&target, input, response_handling)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        result.target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(result.target_thread_id);
                    audit_item.resumed_target = result.resumed_target;
                    audit_item.error = result.post_admission_warning.clone();
                    Ok(AgentControlOutcome::Prompted {
                        target_thread_id: result.target_thread_id.to_string(),
                        submission_id: result.submission_id,
                        post_admission_warning: result.post_admission_warning,
                    })
                }
                AgentControlAction::QueuedPrompt {
                    target,
                    input,
                    response_handling,
                } => {
                    validate_user_input_image_urls(&input)?;
                    validate_v2_input_limit(&input)?;
                    let input = input.into_iter().map(V2UserInput::into_core).collect();
                    let response_handling = response_handling
                        .map(user_agent_response_handling)
                        .unwrap_or_default();
                    let result = source_thread
                        .prompt_idle_agent(&target, input, response_handling)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        result.target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(result.target_thread_id);
                    audit_item.resumed_target = result.resumed_target;
                    audit_item.error = result.post_admission_warning.clone();
                    Ok(AgentControlOutcome::Prompted {
                        target_thread_id: result.target_thread_id.to_string(),
                        submission_id: result.submission_id,
                        post_admission_warning: result.post_admission_warning,
                    })
                }
                AgentControlAction::ReservedPrompt { target, input } => {
                    validate_user_input_image_urls(&input)?;
                    validate_v2_input_limit(&input)?;
                    let input = input.into_iter().map(V2UserInput::into_core).collect();
                    let result = source_thread
                        .prompt_live_agent_using_reserved_response(&target, input)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        result.target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(result.target_thread_id);
                    let final_response = match result.response_handling.final_response() {
                        codex_core::UserAgentFinalResponseHandling::None => {
                            AgentFinalResponseHandling::None
                        }
                        codex_core::UserAgentFinalResponseHandling::Passive => {
                            AgentFinalResponseHandling::Passive
                        }
                        codex_core::UserAgentFinalResponseHandling::Wake => {
                            AgentFinalResponseHandling::Wake
                        }
                        codex_core::UserAgentFinalResponseHandling::Presentation => {
                            AgentFinalResponseHandling::Presentation
                        }
                    };
                    conversion::apply_agent_control_response_handling(
                        &mut audit_item,
                        Some(AgentResponseHandling::new(
                            result.response_handling.commentary(),
                            final_response,
                            result.response_handling.target_messages(),
                            result.response_handling.queue_input(),
                        )),
                        /*queued*/ false,
                    );
                    audit_item.error = result.post_admission_warning.clone();
                    Ok(AgentControlOutcome::ReservedPrompted {
                        target_thread_id: result.target_thread_id.to_string(),
                        submission_id: result.submission_id,
                        turn_id: result.target_turn_id,
                        post_admission_warning: result.post_admission_warning,
                    })
                }
                AgentControlAction::Resume {
                    target,
                    response_handling,
                } => {
                    let response_handling = response_handling
                        .map(user_agent_response_handling)
                        .unwrap_or_default();
                    let result = source_thread
                        .resume_agent(&target, response_handling)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        result.target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(result.target_thread_id);
                    if let Some(ownership_transfer) = result.ownership_transfer {
                        audit_item.previous_owner_session_id =
                            ownership_transfer.previous_session_id;
                        audit_item.new_owner_session_id = Some(ownership_transfer.new_session_id);
                    }
                    audit_item.error = result.post_commit_warning.clone();
                    Ok(AgentControlOutcome::Resumed {
                        target_thread_id: result.target_thread_id.to_string(),
                        agent_ref: result.agent_ref.map(|agent_ref| agent_ref.to_string()),
                        nickname: result.nickname,
                        observation_binding: result
                            .observation_binding
                            .map(agent_observation_binding),
                        post_commit_warning: result.post_commit_warning,
                    })
                }
                AgentControlAction::Interrupt {
                    target,
                    input,
                    response_handling,
                } => {
                    if input.is_none() && response_handling.is_some() {
                        return Err(invalid_request(
                            "responseHandling requires an interrupt follow-up input",
                        ));
                    }
                    let follow_up = input
                        .map(|input| {
                            validate_user_input_image_urls(&input)?;
                            validate_v2_input_limit(&input)?;
                            let input = input.into_iter().map(V2UserInput::into_core).collect();
                            let response_handling = response_handling
                                .map(user_agent_response_handling)
                                .unwrap_or_default();
                            Ok((input, response_handling))
                        })
                        .transpose()?;
                    let (target_thread_id, result) = source_thread
                        .interrupt_agent(&target, follow_up)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(target_thread_id);
                    audit_item.error = result
                        .as_ref()
                        .and_then(|result| result.post_admission_warning.clone());
                    Ok(AgentControlOutcome::Interrupted {
                        target_thread_id: target_thread_id.to_string(),
                        submission_id: result.as_ref().map(|result| result.submission_id.clone()),
                        post_admission_warning: result
                            .and_then(|result| result.post_admission_warning),
                    })
                }
                AgentControlAction::Close { target } => {
                    let target_thread_id = source_thread
                        .close_agent(&target)
                        .await
                        .map_err(agent_control_error)?;
                    audit_item.target_thread_id = Some(target_thread_id);
                    Ok(AgentControlOutcome::Closed {
                        target_thread_id: target_thread_id.to_string(),
                    })
                }
                AgentControlAction::Observe {
                    target,
                    response_handling,
                } => {
                    let response_handling = user_agent_final_response_handling(response_handling);
                    let (target_thread_id, previous_response_handling, binding) = source_thread
                        .observe_agent(&target, response_handling)
                        .await
                        .map_err(agent_control_error)?;
                    self.try_attach_thread_listener(
                        target_thread_id,
                        vec![request_id.connection_id],
                    )
                    .await;
                    audit_item.target_thread_id = Some(target_thread_id);
                    Ok(AgentControlOutcome::Observed {
                        target_thread_id: target_thread_id.to_string(),
                        previous_response_handling: agent_final_response_handling(
                            previous_response_handling,
                        ),
                        response_handling: observation_mode_final_response_handling(
                            response_handling,
                        ),
                        binding: agent_observation_binding(binding),
                    })
                }
            }
        }
        .await;

        match operation {
            Ok(outcome) => {
                if let Some(target_thread_id) = audit_item.target_thread_id {
                    if let Some(state_db) = self.state_db.as_ref() {
                        match state_db
                            .find_current_agent_alias_by_thread(target_thread_id)
                            .await
                        {
                            Ok(Some(alias)) => {
                                audit_item.agent_ref = Some(alias.agent_ref);
                                audit_item.nickname = alias.nickname.or(audit_item.nickname);
                            }
                            Ok(None) => {}
                            Err(err) => {
                                warn!(
                                    "failed to enrich user agent control audit alias for \
                                     {target_thread_id}: {err}"
                                );
                            }
                        }
                    }
                    if let Ok(target) = self
                        .read_thread_view(target_thread_id, /*include_turns*/ false)
                        .await
                    {
                        audit_item.nickname = audit_item.nickname.or(target.agent_nickname);
                        audit_item.role = audit_item.role.or(target.agent_role);
                    }
                }

                let audit_warning = source_thread
                    .record_user_agent_control(audit_item)
                    .await
                    .err()
                    .map(|err| {
                        format!(
                            "agent control succeeded, but its source audit record could not be \
                             persisted: {err}"
                        )
                    });
                Ok(AgentControlResponse {
                    outcome,
                    audit_warning,
                })
            }
            Err(error)
                if error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    == Some("targetActive") =>
            {
                // This is an atomic queue-admission deferral, not a failed user action. The TUI
                // keeps the process-local item queued and retries after the active turn stops.
                Err(error)
            }
            Err(error) => {
                audit_item.status = CoreUserAgentControlStatus::Failed;
                audit_item.error = Some(error.message.clone());
                if let Err(audit_error) = source_thread.record_user_agent_control(audit_item).await
                {
                    warn!(
                        "failed to persist rejected user agent control audit record for \
                         {source_thread_id}: {audit_error}"
                    );
                }
                Err(error)
            }
        }
    }
}

fn agent_control_action_target(action: &AgentControlAction) -> Option<&str> {
    match action {
        AgentControlAction::Spawn { .. } => None,
        AgentControlAction::Prompt { target, .. }
        | AgentControlAction::ReservedPrompt { target, .. }
        | AgentControlAction::QueuedPrompt { target, .. }
        | AgentControlAction::Resume { target, .. }
        | AgentControlAction::Interrupt { target, .. }
        | AgentControlAction::Close { target }
        | AgentControlAction::Observe { target, .. } => Some(target),
    }
}
