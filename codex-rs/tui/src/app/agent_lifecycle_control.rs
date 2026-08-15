//! User-authored agent spawn, interruption, observation, resume, and close commands.

use codex_app_server_protocol::AgentResponseHandling;

use super::agent_observation_display::AgentResponseObservationBinding;
use super::*;
use crate::chatwidget::UserMessage;
use crate::chatwidget::agent_command::AgentSelector;

impl App {
    pub(super) async fn spawn_agent_from_command(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        role: Option<String>,
        authored_selector: Option<String>,
        prompt: Option<UserMessage>,
        fork_mode: codex_app_server_protocol::AgentForkMode,
        response_handling: Option<AgentResponseHandling>,
    ) -> Option<ThreadId> {
        let starts_turn = prompt.is_some();
        let agent_role = role.clone();
        let input = prompt
            .as_ref()
            .map(|message| self.chat_widget.user_inputs_from_message(message));
        let result = app_server
            .spawn_agent(
                source_thread_id,
                role,
                authored_selector,
                input,
                fork_mode,
                response_handling,
            )
            .await;
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to spawn agent: {error:#}"));
                return None;
            }
        };
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Spawned {
                target_thread_id,
                agent_ref,
                nickname,
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
                self.refresh_primary_agent_aliases(app_server).await;
                self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
                    .await;
                let binding = if starts_turn {
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
                if let Some(audit_warning) = audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was spawned, but its source audit failed; do not \
                         retry the spawn: {audit_warning}"
                    ));
                }
                if let Some(warning) = post_admission_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was spawned and its prompt was admitted, but \
                         response handling degraded; do not retry the spawn: {warning}"
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

    pub(super) async fn interrupt_agent_from_selector(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        selector: AgentSelector,
        follow_up: Option<UserMessage>,
        response_handling: Option<AgentResponseHandling>,
    ) {
        let target = match selector.control_target() {
            Ok(target) => target,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        let input = follow_up
            .as_ref()
            .map(|message| self.chat_widget.user_inputs_from_message(message));
        let result = app_server
            .interrupt_agent(
                source_thread_id,
                target,
                selector.authored().to_string(),
                input,
                response_handling,
            )
            .await;
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to interrupt agent: {error:#}"));
                return;
            }
        };
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Interrupted {
                target_thread_id,
                submission_id,
                post_admission_warning,
            } => {
                if let Ok(target_thread_id) = ThreadId::from_string(&target_thread_id) {
                    self.refresh_primary_agent_aliases(app_server).await;
                    self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
                        .await;
                    if submission_id.is_some()
                        && post_admission_warning.is_none()
                        && self.agent_navigation.is_running(target_thread_id)
                    {
                        self.agent_navigation.note_response_observation(
                            source_thread_id,
                            target_thread_id,
                            AgentResponseObservationBinding::Bound,
                            response_handling,
                        );
                    }
                }
                if let Some(audit_warning) = audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was interrupted, but its source audit failed; do \
                         not retry the action: {audit_warning}"
                    ));
                }
                if let Some(warning) = post_admission_warning {
                    self.chat_widget.add_error_message(format!(
                        "The interrupt follow-up was admitted to agent {target_thread_id}, but \
                         response handling degraded; do not retry it: {warning}"
                    ));
                }
            }
            _ => {
                self.chat_widget.add_error_message(
                    "Agent interrupt returned an unexpected response.".to_string(),
                );
            }
        }
    }

    pub(super) async fn close_agent_from_selector(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        selector: AgentSelector,
    ) {
        let target = match selector.control_target() {
            Ok(target) => target,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        let result = app_server
            .close_agent(source_thread_id, target, selector.authored().to_string())
            .await;
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to close agent: {error:#}"));
                return;
            }
        };
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Closed { target_thread_id } => {
                if let Ok(target_thread_id) = ThreadId::from_string(&target_thread_id) {
                    self.mark_agent_picker_thread_closed(target_thread_id);
                    self.refresh_primary_agent_aliases(app_server).await;
                    self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
                        .await;
                }
                if let Some(audit_warning) = audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} was closed, but its source audit failed; do not \
                         retry the close: {audit_warning}"
                    ));
                }
            }
            _ => {
                self.chat_widget
                    .add_error_message("Agent close returned an unexpected response.".to_string());
            }
        }
    }

    pub(super) async fn observe_agent_from_selector(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        selector: AgentSelector,
        response_handling: codex_app_server_protocol::AgentObservationMode,
    ) {
        let target = match selector.control_target() {
            Ok(target) => target,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        let result = app_server
            .observe_agent(
                source_thread_id,
                target,
                selector.authored().to_string(),
                response_handling,
            )
            .await;
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to change agent observation: {error:#}"));
                return;
            }
        };
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Observed {
                target_thread_id,
                previous_response_handling: _,
                response_handling,
                binding,
            } => {
                if let Ok(target_thread_id) = ThreadId::from_string(&target_thread_id) {
                    self.refresh_primary_agent_aliases(app_server).await;
                    self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
                        .await;
                    let binding = match binding {
                        codex_app_server_protocol::AgentObservationBinding::ActiveTurn => {
                            Some(AgentResponseObservationBinding::Bound)
                        }
                        codex_app_server_protocol::AgentObservationBinding::NextTurn => {
                            Some(AgentResponseObservationBinding::NextTurn)
                        }
                        codex_app_server_protocol::AgentObservationBinding::UndeliveredCompletion => {
                            None
                        }
                    };
                    if let Some(binding) = binding {
                        self.agent_navigation
                            .replace_user_final_response_observation(
                                source_thread_id,
                                target_thread_id,
                                binding,
                                response_handling,
                            );
                    } else {
                        self.agent_navigation
                            .clear_response_observation(source_thread_id, target_thread_id);
                    }
                }
                if let Some(audit_warning) = audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Observation for agent {target_thread_id} changed, but its source audit \
                         failed; do not retry the change: {audit_warning}"
                    ));
                }
            }
            _ => {
                self.chat_widget.add_error_message(
                    "Agent observe returned an unexpected response.".to_string(),
                );
            }
        }
    }

    pub(super) async fn resume_agent_from_selector(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        selector: AgentSelector,
        response_handling: Option<AgentResponseHandling>,
        prompt: Option<UserMessage>,
    ) {
        let target = match selector.control_target() {
            Ok(target) => target,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        let result = app_server
            .resume_agent(
                source_thread_id,
                target,
                selector.authored().to_string(),
                response_handling,
            )
            .await;
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to resume agent: {error:#}"));
                return;
            }
        };
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Resumed {
                target_thread_id,
                agent_ref,
                nickname,
                observation_binding,
                post_commit_warning,
            } => {
                let Ok(target_thread_id) = ThreadId::from_string(&target_thread_id) else {
                    self.chat_widget.add_error_message(
                        "Agent resume returned an invalid target thread id.".to_string(),
                    );
                    return;
                };
                let resume_degraded = post_commit_warning.is_some();
                self.upsert_agent_picker_thread(
                    target_thread_id,
                    nickname.clone(),
                    /*agent_role*/ None,
                    /*is_closed*/ resume_degraded,
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
                            if resume_degraded {
                                codex_app_server_protocol::AgentAliasState::Closed
                            } else {
                                codex_app_server_protocol::AgentAliasState::Active
                            },
                        ),
                        None => tracing::warn!(
                            %target_thread_id,
                            agent_ref,
                            "agent resume returned an invalid durable ref"
                        ),
                    }
                }
                self.refresh_primary_agent_aliases(app_server).await;
                self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
                    .await;
                let binding = if resume_degraded {
                    None
                } else {
                    match observation_binding {
                        Some(codex_app_server_protocol::AgentObservationBinding::ActiveTurn) => {
                            Some(AgentResponseObservationBinding::Bound)
                        }
                        Some(codex_app_server_protocol::AgentObservationBinding::NextTurn) => {
                            Some(AgentResponseObservationBinding::NextTurn)
                        }
                        Some(
                            codex_app_server_protocol::AgentObservationBinding::UndeliveredCompletion,
                        )
                        | None => None,
                    }
                };
                if let Some(binding) = binding {
                    self.agent_navigation.note_response_observation(
                        source_thread_id,
                        target_thread_id,
                        binding,
                        response_handling,
                    );
                } else {
                    self.agent_navigation
                        .clear_response_observation(source_thread_id, target_thread_id);
                    self.agent_navigation
                        .clear_reserved_prompt_response(target_thread_id);
                }
                if binding == Some(AgentResponseObservationBinding::NextTurn) {
                    self.agent_navigation
                        .reserve_prompt_response(source_thread_id, target_thread_id);
                }
                if let Some(warning) = post_commit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Agent ownership changed, but resume setup degraded: {warning}"
                    ));
                }
                if let Some(audit_warning) = audit_warning {
                    let retry_guidance = if resume_degraded {
                        "follow the resume recovery guidance above"
                    } else {
                        "do not retry the resume"
                    };
                    self.chat_widget.add_error_message(format!(
                        "Agent {target_thread_id} resume outcome committed, but its source audit \
                         failed; {retry_guidance}: {audit_warning}"
                    ));
                }
                self.sync_active_agent_label();
                if !resume_degraded && let Some(prompt) = prompt {
                    self.submit_agent_prompt_to_selector(
                        app_server,
                        source_thread_id,
                        selector,
                        prompt,
                        /*response_handling*/ None,
                    )
                    .await;
                }
            }
            _ => {
                self.chat_widget
                    .add_error_message("Agent resume returned an unexpected response.".to_string());
            }
        }
    }
}
