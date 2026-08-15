//! Direct user-authored prompts to open agent threads without changing TUI focus.

use codex_app_server_protocol::AgentResponseHandling;
use codex_app_server_protocol::UserInput;

use super::agent_observation_display::AgentResponseObservationBinding;
use super::*;
use crate::chatwidget::UserMessage;
use crate::chatwidget::agent_command::AgentSelector;
use crate::chatwidget::agent_command::AgentSelectorKind;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AgentPromptAvailability {
    Available(String),
    Closed(String),
    Current(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentPromptAdmission {
    Direct,
    Queued,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AgentPromptSubmission {
    Admitted {
        queued: bool,
        audit_warning: Option<String>,
        post_admission_warning: Option<String>,
    },
    Rejected,
}

pub(crate) struct SubmitAgentPromptArgs {
    pub source_thread_id: ThreadId,
    pub thread_id: ThreadId,
    pub target: String,
    pub authored_selector: String,
    pub user_message: UserMessage,
    pub response_handling: Option<AgentResponseHandling>,
    pub admission: AgentPromptAdmission,
}

struct SubmitAgentPromptItemsArgs {
    source_thread_id: ThreadId,
    thread_id: ThreadId,
    target: String,
    authored_selector: String,
    items: Vec<UserInput>,
    response_handling: Option<AgentResponseHandling>,
    admission: AgentPromptAdmission,
}

impl AgentPromptAvailability {
    pub(super) fn into_label(self) -> Option<String> {
        match self {
            Self::Available(label) | Self::Closed(label) | Self::Current(label) => Some(label),
            Self::Unknown => None,
        }
    }
}

impl App {
    pub(super) async fn resolve_agent_selector(
        &mut self,
        app_server: &AppServerSession,
        selector: &AgentSelector,
    ) -> Result<ThreadId, String> {
        if let Some(thread_id) = self.cached_agent_selector(selector)? {
            return Ok(thread_id);
        }
        if matches!(
            selector.kind(),
            AgentSelectorKind::Ref(_)
                | AgentSelectorKind::Nickname(_)
                | AgentSelectorKind::UnprefixedName(_)
        ) && let Some(root_thread_id) = self.primary_thread_id
        {
            let aliases = app_server
                .agent_aliases(root_thread_id)
                .await
                .map_err(|err| format!("Failed to load agent aliases: {err:#}"))?;
            self.apply_primary_agent_aliases(aliases);
            if let Some(thread_id) = self.cached_agent_selector(selector)? {
                return Ok(thread_id);
            }
        }
        Err(match selector.kind() {
            AgentSelectorKind::Id(thread_id) => format!("Agent {thread_id} was not found."),
            AgentSelectorKind::Ref(agent_ref) => format!("Agent ref {agent_ref} was not found."),
            AgentSelectorKind::Nickname(nickname) | AgentSelectorKind::UnprefixedName(nickname) => {
                format!("Agent {nickname:?} was not found.")
            }
            AgentSelectorKind::Role(role) => format!("Agent role {role:?} was not found."),
        })
    }

    pub(super) async fn submit_agent_prompt_to_selector(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        selector: AgentSelector,
        user_message: UserMessage,
        response_handling: Option<AgentResponseHandling>,
    ) {
        let target = match selector.control_target() {
            Ok(target) => target,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        let thread_id = match self.resolve_agent_selector(app_server, &selector).await {
            Ok(thread_id) => thread_id,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                return;
            }
        };
        self.submit_agent_prompt_with_control(
            app_server,
            SubmitAgentPromptArgs {
                source_thread_id,
                thread_id,
                target,
                authored_selector: selector.authored().to_string(),
                user_message,
                response_handling,
                admission: AgentPromptAdmission::Direct,
            },
        )
        .await;
    }

    pub(super) async fn submit_reserved_agent_prompt(
        &mut self,
        app_server: &mut AppServerSession,
        source_thread_id: ThreadId,
        target_thread_id: ThreadId,
        input: Vec<UserInput>,
    ) -> Result<String> {
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = app_server
            .prompt_agent_reserved_turn(source_thread_id, target_thread_id, input)
            .await?;
        let codex_app_server_protocol::AgentControlOutcome::ReservedPrompted {
            target_thread_id: prompted_thread_id,
            turn_id,
            post_admission_warning,
            ..
        } = outcome
        else {
            return Err(color_eyre::eyre::eyre!(
                "reserved agent prompt returned an unexpected response"
            ));
        };
        let prompted_thread_id = ThreadId::from_string(&prompted_thread_id)
            .wrap_err("reserved agent prompt returned an invalid target thread id")?;
        if prompted_thread_id != target_thread_id {
            return Err(color_eyre::eyre::eyre!(
                "reserved agent prompt targeted {prompted_thread_id}, expected {target_thread_id}"
            ));
        }
        // Admission consumed the next-turn reservation. Promote its display binding before the
        // liveness refresh so a turn that already completed clears it instead of preserving it for
        // another future turn.
        self.agent_navigation.mark_running(target_thread_id);
        if post_admission_warning.is_some() {
            self.agent_navigation
                .clear_response_observation(source_thread_id, target_thread_id);
        }
        self.refresh_agent_picker_thread_liveness(app_server, target_thread_id)
            .await;
        self.sync_active_agent_label();
        if let Some(warning) = post_admission_warning {
            self.chat_widget.add_error_message(format!(
                "The first prompt was admitted to agent {target_thread_id}, but response handling \
                 degraded; do not retry the prompt: {warning}"
            ));
        }
        if let Some(warning) = audit_warning {
            self.chat_widget.add_error_message(format!(
                "The first agent prompt was admitted, but its source audit failed; do not retry \
                 it: {warning}"
            ));
        }
        Ok(turn_id)
    }

    fn cached_agent_selector(&self, selector: &AgentSelector) -> Result<Option<ThreadId>, String> {
        match selector.kind() {
            AgentSelectorKind::Id(thread_id) => Ok(Some(*thread_id)),
            AgentSelectorKind::Ref(agent_ref) => {
                Ok(self.agent_navigation.thread_id_for_ref(*agent_ref))
            }
            AgentSelectorKind::Nickname(nickname) => {
                Ok(self.agent_navigation.thread_id_for_nickname(nickname))
            }
            AgentSelectorKind::UnprefixedName(name) => {
                if self.config.agent_roles.contains_key(name) {
                    return Err(format!(
                        "{name:?} is a configured role; add a prompt to spawn a new role agent"
                    ));
                }
                Ok(self.agent_navigation.thread_id_for_nickname(name))
            }
            AgentSelectorKind::Role(role) => Err(format!(
                "{role:?} selects a configured role; add a prompt to spawn a new role agent"
            )),
        }
    }

    #[cfg(test)]
    pub(super) async fn submit_agent_prompt(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        user_message: UserMessage,
    ) {
        let Some(source_thread_id) = self.current_displayed_thread_id() else {
            self.chat_widget
                .add_error_message("No displayed source thread is available.".to_string());
            return;
        };
        self.submit_agent_prompt_with_control(
            app_server,
            SubmitAgentPromptArgs {
                source_thread_id,
                thread_id,
                target: thread_id.to_string(),
                authored_selector: thread_id.to_string(),
                user_message,
                response_handling: None,
                admission: AgentPromptAdmission::Direct,
            },
        )
        .await;
    }

    pub(super) async fn submit_agent_prompt_with_control(
        &mut self,
        app_server: &mut AppServerSession,
        args: SubmitAgentPromptArgs,
    ) -> AgentPromptSubmission {
        let SubmitAgentPromptArgs {
            source_thread_id,
            thread_id,
            target,
            authored_selector,
            user_message,
            response_handling,
            admission,
        } = args;
        match self.agent_prompt_availability(thread_id) {
            AgentPromptAvailability::Current(label) => {
                self.chat_widget.add_error_message(format!(
                    "Already viewing {label}; submit the prompt normally."
                ));
                return AgentPromptSubmission::Rejected;
            }
            AgentPromptAvailability::Available(_)
            | AgentPromptAvailability::Closed(_)
            | AgentPromptAvailability::Unknown => {}
        }

        // A live local event attachment can outlast the app-server thread when a close
        // notification is still queued behind this user action. `thread/read` does not load or
        // resume a thread, so use it as the authoritative submission-time liveness check.
        self.refresh_agent_picker_thread_liveness(app_server, thread_id)
            .await;
        self.sync_active_agent_label();
        let label = match self.agent_prompt_availability(thread_id) {
            AgentPromptAvailability::Available(label) | AgentPromptAvailability::Closed(label) => {
                label
            }
            AgentPromptAvailability::Current(label) => {
                self.chat_widget.add_error_message(format!(
                    "Already viewing {label}; submit the prompt normally."
                ));
                return AgentPromptSubmission::Rejected;
            }
            AgentPromptAvailability::Unknown => thread_id.to_string(),
        };

        let items = self.chat_widget.user_inputs_from_message(&user_message);
        if items.is_empty() {
            self.chat_widget
                .add_error_message("The agent prompt is empty.".to_string());
            return AgentPromptSubmission::Rejected;
        }
        if admission == AgentPromptAdmission::Direct
            && response_handling.is_none()
            && self.agent_navigation.reserved_prompt_source(thread_id) == Some(source_thread_id)
        {
            return match self
                .submit_reserved_agent_prompt(app_server, source_thread_id, thread_id, items)
                .await
            {
                Ok(_) => AgentPromptSubmission::Admitted {
                    queued: false,
                    audit_warning: None,
                    post_admission_warning: None,
                },
                Err(error) => {
                    self.chat_widget.add_error_message(format!(
                        "Failed to submit the first prompt to {label}: {error:#}"
                    ));
                    AgentPromptSubmission::Rejected
                }
            };
        }
        match self
            .submit_agent_prompt_items(
                app_server,
                SubmitAgentPromptItemsArgs {
                    source_thread_id,
                    thread_id,
                    target,
                    authored_selector,
                    items,
                    response_handling,
                    admission,
                },
            )
            .await
        {
            Ok(AgentPromptSubmission::Admitted {
                queued,
                audit_warning,
                post_admission_warning,
            }) => {
                self.refresh_primary_agent_aliases(app_server).await;
                self.refresh_agent_picker_thread_liveness(app_server, thread_id)
                    .await;
                if !queued
                    && post_admission_warning.is_none()
                    && self.agent_navigation.is_running(thread_id)
                {
                    self.agent_navigation.note_response_observation(
                        source_thread_id,
                        thread_id,
                        AgentResponseObservationBinding::Bound,
                        response_handling,
                    );
                }
                self.sync_active_agent_label();
                if let Some(warning) = &audit_warning {
                    self.chat_widget.add_error_message(format!(
                        "Prompt was admitted to {label}, but its source audit failed; do not retry \
                         it: {warning}"
                    ));
                }
                if let Some(warning) = &post_admission_warning {
                    self.chat_widget.add_error_message(format!(
                        "Prompt was admitted to {label}, but response handling degraded; do not \
                         retry it: {warning}"
                    ));
                }
                AgentPromptSubmission::Admitted {
                    queued,
                    audit_warning,
                    post_admission_warning,
                }
            }
            Ok(AgentPromptSubmission::Rejected) => {
                unreachable!("submission helper never returns rejected")
            }
            Err(error) => {
                tracing::warn!(
                    target_thread_id = %thread_id,
                    %error,
                    "failed to submit user prompt to agent"
                );
                // The target can close after the preflight read but before turn/steer or
                // turn/start reaches the server. Refresh without loading it and prefer the
                // actionable closed-target guidance over the stale RPC error.
                self.refresh_agent_picker_thread_liveness(app_server, thread_id)
                    .await;
                self.sync_active_agent_label();
                let error_detail = format!("{error:#}").replace(
                    "use resume_agent to adopt it",
                    &format!("run `/agent resume {thread_id}` to adopt it"),
                );
                match self.agent_prompt_availability(thread_id) {
                    AgentPromptAvailability::Closed(label) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to resume and prompt {label}: {error_detail}"
                        ));
                    }
                    AgentPromptAvailability::Unknown => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to prompt agent {thread_id}: {error_detail}"
                        ));
                    }
                    AgentPromptAvailability::Available(_) | AgentPromptAvailability::Current(_) => {
                        self.chat_widget
                            .add_error_message(format!("Failed to prompt {label}: {error_detail}"));
                    }
                }
                AgentPromptSubmission::Rejected
            }
        }
    }

    pub(super) fn agent_prompt_availability(&self, thread_id: ThreadId) -> AgentPromptAvailability {
        agent_prompt_availability(
            &self.agent_navigation,
            self.current_displayed_thread_id(),
            self.agent_root_thread_id(),
            thread_id,
        )
    }

    async fn submit_agent_prompt_items(
        &mut self,
        app_server: &mut AppServerSession,
        args: SubmitAgentPromptItemsArgs,
    ) -> Result<AgentPromptSubmission> {
        let SubmitAgentPromptItemsArgs {
            source_thread_id,
            thread_id,
            target,
            authored_selector,
            items,
            response_handling,
            admission,
        } = args;
        let response = match admission {
            AgentPromptAdmission::Direct => {
                app_server
                    .prompt_agent(
                        source_thread_id,
                        target,
                        authored_selector,
                        items,
                        response_handling,
                    )
                    .await?
            }
            AgentPromptAdmission::Queued => {
                app_server
                    .queue_agent_prompt(
                        source_thread_id,
                        target,
                        authored_selector,
                        items,
                        response_handling,
                    )
                    .await?
            }
        };
        let codex_app_server_protocol::AgentControlResponse {
            outcome,
            audit_warning,
        } = response;
        match outcome {
            codex_app_server_protocol::AgentControlOutcome::Prompted {
                target_thread_id,
                submission_id: _,
                queued,
                post_admission_warning,
            } if ThreadId::from_string(&target_thread_id).ok() == Some(thread_id) => {
                Ok(AgentPromptSubmission::Admitted {
                    queued,
                    audit_warning,
                    post_admission_warning,
                })
            }
            response => Err(color_eyre::eyre::eyre!(
                "agent/control prompt returned {response:?}, expected target {thread_id}"
            )),
        }
    }
}

fn agent_prompt_availability(
    navigation: &AgentNavigationState,
    current_thread_id: Option<ThreadId>,
    primary_thread_id: Option<ThreadId>,
    thread_id: ThreadId,
) -> AgentPromptAvailability {
    let Some(entry) = navigation.get(&thread_id) else {
        return if current_thread_id == Some(thread_id) {
            AgentPromptAvailability::Current(if primary_thread_id == Some(thread_id) {
                "Main [default]".to_string()
            } else {
                "the current agent".to_string()
            })
        } else {
            AgentPromptAvailability::Unknown
        };
    };
    let label = format_agent_picker_item_name(
        entry.agent_nickname.as_deref(),
        entry.agent_role.as_deref(),
        primary_thread_id == Some(thread_id),
    );
    if entry.is_closed {
        AgentPromptAvailability::Closed(label)
    } else if current_thread_id == Some(thread_id) {
        AgentPromptAvailability::Current(label)
    } else {
        AgentPromptAvailability::Available(label)
    }
}

#[cfg(test)]
#[path = "agent_prompt_tests.rs"]
mod tests;
