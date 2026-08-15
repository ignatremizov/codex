//! Contextual controls opened from the `/agent` overview.

use codex_app_server_protocol::AgentAliasState;
use codex_protocol::ThreadId;

use super::App;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

pub(super) const AGENT_ACTIONS_VIEW_ID: &str = "agent-actions";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentControlActionKind {
    InspectTranscript,
    Prompt,
    Queue,
    Interrupt,
    Resume,
    Observe,
    Close,
}

impl AgentControlActionKind {
    const ALL: [Self; 7] = [
        Self::InspectTranscript,
        Self::Prompt,
        Self::Queue,
        Self::Interrupt,
        Self::Resume,
        Self::Observe,
        Self::Close,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::InspectTranscript => "Inspect transcript",
            Self::Prompt => "Prompt",
            Self::Queue => "Queue follow-up",
            Self::Interrupt => "Interrupt turn",
            Self::Resume => "Resume agent",
            Self::Observe => "Observe response",
            Self::Close => "Close agent",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::InspectTranscript => "Open its canonical transcript over this pane",
            Self::Prompt => "Send user input without switching threads",
            Self::Queue => "Run after the active turn, or immediately while idle",
            Self::Interrupt => "Stop the active turn, optionally with a follow-up",
            Self::Resume => "Reopen this controlled agent",
            Self::Observe => "Choose passive, wake, or presentation delivery",
            Self::Close => "End the agent runtime and revoke observation",
        }
    }

    fn disabled_reason(self, state: AgentControlTargetState) -> Option<&'static str> {
        match self {
            Self::InspectTranscript => None,
            Self::Prompt | Self::Queue if state.is_side_thread => {
                Some("Use the normal composer inside this side conversation.")
            }
            Self::Prompt | Self::Queue if state.needs_adoption => {
                Some("Resume by UUID to adopt this agent first.")
            }
            Self::Prompt | Self::Queue if state.is_current => {
                Some("Use the normal composer for the current agent.")
            }
            Self::Prompt | Self::Queue => None,
            Self::Interrupt if state.is_side_thread => {
                Some("Switch to the side conversation to interrupt it.")
            }
            Self::Interrupt if state.needs_adoption => {
                Some("Agent is not controlled by this root.")
            }
            Self::Interrupt if state.is_current => {
                Some("Use the normal interrupt shortcut for the current agent.")
            }
            Self::Interrupt if state.is_closed => Some("Agent is closed."),
            Self::Interrupt if !state.is_running => Some("Agent has no active turn."),
            Self::Interrupt => None,
            Self::Resume if state.is_side_thread => {
                Some("Side conversations use the normal TUI lifecycle.")
            }
            Self::Resume if !state.is_closed && !state.needs_adoption => {
                Some("Agent is already open.")
            }
            Self::Resume => None,
            Self::Observe if state.is_side_thread => {
                Some("Side conversations do not use agent response observation.")
            }
            Self::Observe if state.needs_adoption => Some("Agent is not controlled by this root."),
            Self::Observe if state.is_current => Some("An agent cannot observe itself."),
            Self::Observe if state.is_closed => Some("Resume the closed agent first."),
            Self::Observe => None,
            Self::Close if state.is_side_thread => {
                Some("Side conversations use the normal TUI lifecycle.")
            }
            Self::Close if state.needs_adoption => Some("Agent is not controlled by this root."),
            Self::Close if state.is_primary => Some("Main cannot be closed."),
            Self::Close if state.is_current => Some("An agent cannot close itself."),
            Self::Close if state.is_closed => Some("Agent is already closed."),
            Self::Close => None,
        }
    }

    fn effect(self, thread_id: ThreadId, target: &str) -> AgentControlMenuEffect {
        match self {
            Self::InspectTranscript => AgentControlMenuEffect::InspectTranscript(thread_id),
            Self::Prompt => AgentControlMenuEffect::PrepareCommand(format!("/agent {target} ")),
            Self::Queue => {
                AgentControlMenuEffect::PrepareCommand(format!("/agent queue {target} "))
            }
            Self::Interrupt => {
                AgentControlMenuEffect::PrepareCommand(format!("/agent interrupt {target} "))
            }
            Self::Resume => {
                AgentControlMenuEffect::PrepareCommand(format!("/agent resume {target}"))
            }
            Self::Observe => {
                AgentControlMenuEffect::PrepareCommand(format!("/agent observe {target} "))
            }
            Self::Close => AgentControlMenuEffect::PrepareCommand(format!("/agent close {target}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AgentControlTargetState {
    is_current: bool,
    is_primary: bool,
    is_running: bool,
    is_closed: bool,
    needs_adoption: bool,
    is_side_thread: bool,
}

enum AgentControlMenuEffect {
    InspectTranscript(ThreadId),
    PrepareCommand(String),
}

impl App {
    pub(super) fn open_agent_actions(&mut self, thread_id: ThreadId) {
        let Some(entry) = self.agent_navigation.get(&thread_id) else {
            self.chat_widget
                .add_error_message(format!("Agent {thread_id} was not found."));
            return;
        };
        let alias = self.agent_navigation.alias(thread_id);
        let is_transferred = alias.is_some_and(|alias| alias.state == AgentAliasState::Transferred);
        let agent_root_thread_id = self.agent_root_thread_id();
        let is_primary = agent_root_thread_id == Some(thread_id);
        let has_parent = self.agent_navigation.parent_thread_id(thread_id).is_some();
        let needs_adoption = is_transferred || (!is_primary && alias.is_none() && !has_parent);
        let state = AgentControlTargetState {
            is_current: self.current_displayed_thread_id() == Some(thread_id),
            is_primary,
            is_running: entry.is_running && !entry.is_closed,
            is_closed: entry.is_closed,
            needs_adoption,
            is_side_thread: self.side_threads.contains_key(&thread_id),
        };
        let target = if is_primary {
            Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string())
        } else if needs_adoption {
            None
        } else {
            self.agent_navigation.control_selector(thread_id)
        }
        .unwrap_or_else(|| thread_id.to_string());
        let label = self
            .agent_navigation
            .display_name(thread_id, agent_root_thread_id);
        self.chat_widget
            .show_selection_view(agent_control_actions_view_params(
                thread_id, &target, label, state,
            ));
    }
}

fn agent_control_actions_view_params(
    thread_id: ThreadId,
    target: &str,
    label: String,
    state: AgentControlTargetState,
) -> SelectionViewParams {
    let items = AgentControlActionKind::ALL
        .into_iter()
        .map(|kind| {
            let disabled_reason = kind.disabled_reason(state).map(str::to_string);
            let action = kind.effect(thread_id, &target);
            SelectionItem {
                name: kind.label().to_string(),
                description: Some(kind.description().to_string()),
                is_disabled: disabled_reason.is_some(),
                disabled_reason,
                actions: vec![Box::new(move |tx| match &action {
                    AgentControlMenuEffect::InspectTranscript(thread_id) => {
                        tx.send(AppEvent::InspectAgentTranscript(*thread_id));
                    }
                    AgentControlMenuEffect::PrepareCommand(command) => {
                        tx.send(AppEvent::PrepareAgentCommand(command.clone()));
                    }
                })],
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect();

    SelectionViewParams {
        view_id: Some(AGENT_ACTIONS_VIEW_ID),
        title: Some(format!("Controls: {label}")),
        subtitle: Some(thread_id.to_string()),
        footer_note: Some(
            "Prepared commands return to the current composer for confirmation.".into(),
        ),
        footer_hint: Some(standard_popup_hint_line()),
        items,
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "agent_control_actions_tests.rs"]
mod tests;
