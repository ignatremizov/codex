//! User-authored agent creation and committed-input recovery.

use super::agent_observation_display::AgentResponseObservationBinding;
use super::*;

impl App {
    pub(super) async fn spawn_agent_from_command(
        &mut self,
        app_server: &mut AppServerSession,
        args: SpawnAgentCommandArgs,
    ) -> Option<ThreadId> {
        let SpawnAgentCommandArgs {
            source_thread_id,
            task,
            role,
            authored_selector,
            model,
            reasoning_effort,
            prompt,
            fork_mode,
            response_handling,
        } = args;
        let response = super::agent_prompt_queue::response_handling_option(response_handling)
            .map(|option| format!(" {option}"))
            .unwrap_or_default();
        let task_option = task
            .as_deref()
            .map(|task| format!(" task:{task}"))
            .unwrap_or_default();
        let fork = match fork_mode {
            codex_app_server_protocol::AgentForkMode::None => "none".to_string(),
            codex_app_server_protocol::AgentForkMode::All => "all".to_string(),
            codex_app_server_protocol::AgentForkMode::LastNTurns { turns } => turns.to_string(),
        };
        let model_option = model.as_deref().map(|model| {
            if model
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '"' | '\\'))
            {
                let model = model.replace('\\', "\\\\").replace('"', "\\\"");
                format!(" model:\"{model}\"")
            } else {
                format!(" model:{model}")
            }
        });
        let reasoning_effort_option = reasoning_effort
            .as_ref()
            .map(|effort| format!(" effort:{effort}"))
            .unwrap_or_default();
        let recovery_command = format!(
            "/agent {}{task_option} fork:{fork}{}{reasoning_effort_option}{response}",
            authored_selector.as_deref().unwrap_or("new"),
            model_option.as_deref().unwrap_or_default()
        );
        if let Err(error) =
            self.ensure_agent_control_admission(source_thread_id, /*target*/ None)
        {
            self.chat_widget.add_error_message(error.to_string());
            if let Some(prompt) = prompt {
                self.recover_agent_command(source_thread_id, recovery_command, prompt);
            }
            return None;
        }
        let starts_turn = prompt.is_some();
        let agent_role = role.clone();
        let input = match prompt.as_ref() {
            Some(message) => match self
                .chat_widget
                .agent_user_inputs_from_message(message)
                .await
            {
                Ok(input) => Some(input),
                Err(error) => {
                    self.chat_widget.add_error_message(error);
                    self.recover_agent_command(source_thread_id, recovery_command, message.clone());
                    return None;
                }
            },
            None => None,
        };
        let result = app_server
            .spawn_agent(crate::app_server_session::SpawnAgentRequest {
                source_thread_id,
                task,
                role,
                authored_selector,
                model,
                reasoning_effort,
                input,
                fork_mode,
                response_handling,
            })
            .await;
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to spawn agent: {error:#}"));
                if let Some(prompt) = prompt {
                    self.recover_agent_command(source_thread_id, recovery_command, prompt);
                }
                return None;
            }
        };
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Spawned {
                target_thread_id,
                agent_ref,
                nickname,
                input_outcome,
                task_path,
                post_admission_warning,
            } => {
                let Ok(target_thread_id) = ThreadId::from_string(&target_thread_id) else {
                    self.chat_widget.add_error_message(
                        "Agent spawn returned an invalid target thread id.".to_string(),
                    );
                    return None;
                };
                self.upsert_agent_picker_thread(
                    target_thread_id,
                    nickname.clone(),
                    agent_role,
                    /*is_closed*/ false,
                );
                if let Some(agent_ref) = agent_ref {
                    match agent_ref
                        .parse::<u64>()
                        .ok()
                        .filter(|agent_ref| *agent_ref > 0)
                    {
                        Some(agent_ref) => self.agent_navigation.upsert_alias(
                            target_thread_id,
                            agent_ref,
                            nickname,
                            codex_app_server_protocol::AgentAliasState::Active,
                        ),
                        None => tracing::warn!(
                            %target_thread_id,
                            agent_ref,
                            "agent spawn returned an invalid durable ref"
                        ),
                    }
                }
                self.agent_navigation
                    .update_task_path(target_thread_id, task_path);
                self.refresh_primary_agent_aliases(app_server).await;
                self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
                    .await;
                let input_unknown =
                    input_outcome == Some(codex_app_server_protocol::AgentInputOutcome::Unknown);
                let binding = if input_unknown
                    || input_outcome == Some(codex_app_server_protocol::AgentInputOutcome::Queued)
                {
                    None
                } else if starts_turn {
                    self.agent_navigation
                        .is_running(target_thread_id)
                        .then_some(AgentResponseObservationBinding::Bound)
                } else {
                    Some(AgentResponseObservationBinding::NextTurn)
                };
                if post_admission_warning.is_none()
                    && let Some(binding) = binding
                {
                    self.agent_navigation.note_response_observation(
                        source_thread_id,
                        target_thread_id,
                        binding,
                        response_handling,
                    );
                }
                if !starts_turn && post_admission_warning.is_none() {
                    self.agent_navigation
                        .reserve_prompt_response(source_thread_id, target_thread_id);
                }
                if input_unknown && let Some(prompt) = prompt {
                    self.recover_agent_command(
                        source_thread_id,
                        format!("/agent {target_thread_id}"),
                        prompt,
                    );
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was created, but its input outcome is unknown. \
                         The prompt is retained; reload the target transcript before retrying."
                    ));
                }
                if let Some(audit_warning) = audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was spawned, but its source audit failed; do not \
                         retry the spawn: {audit_warning}"
                    ));
                }
                if let Some(warning) = post_admission_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was spawned with an input delivery warning; \
                         do not retry the spawn: {warning}"
                    ));
                }
                self.sync_active_agent_label();
                Some(target_thread_id)
            }
            _ => {
                self.chat_widget
                    .add_error_message("Agent spawn returned an unexpected response.".to_string());
                None
            }
        }
    }
}
