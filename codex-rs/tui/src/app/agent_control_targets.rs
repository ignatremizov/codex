//! Root-scoped command completion without changing the displayed conversation.

use super::*;

impl App {
    pub(super) fn agent_root_thread_id(&self) -> Option<ThreadId> {
        self.agent_navigation
            .root_thread_id()
            .or(self.primary_thread_id)
    }

    pub(super) fn sync_agent_prompt_targets(&mut self) {
        let current_thread_id = self.current_displayed_thread_id();
        let root = self.agent_root_thread_id();
        let mut targets = self
            .agent_navigation
            .ordered_threads()
            .into_iter()
            .filter(|(thread_id, _)| {
                Some(*thread_id) != current_thread_id
                    && self.agent_navigation.alias(*thread_id).is_none_or(|alias| {
                        alias.state != codex_app_server_protocol::AgentAliasState::Transferred
                    })
            })
            .map(|(thread_id, entry)| {
                let is_primary = root == Some(thread_id);
                let label = format_agent_picker_item_name(
                    entry.agent_nickname.as_deref(),
                    entry.agent_role.as_deref(),
                    is_primary,
                );
                AgentPromptTarget {
                    thread_id: Some(thread_id),
                    selector: if is_primary {
                        codex_protocol::MAIN_AGENT_NICKNAME.to_string()
                    } else {
                        self.agent_navigation
                            .control_selector(thread_id)
                            .unwrap_or_else(|| thread_id.to_string())
                    },
                    label: if entry.is_closed {
                        format!("{label} · closed")
                    } else {
                        label
                    },
                }
            })
            .collect::<Vec<_>>();
        targets.push(AgentPromptTarget {
            thread_id: None,
            selector: "new".to_string(),
            label: "New default agent".to_string(),
        });
        targets.extend(
            AGENT_TARGET_ACTION_CHOICES.map(|(selector, label)| AgentPromptTarget {
                thread_id: None,
                selector: selector.to_string(),
                label: label.to_string(),
            }),
        );
        targets.extend(self.config.agent_roles.keys().map(|role| {
            let reserved = role == "new"
                || role.eq_ignore_ascii_case(codex_protocol::MAIN_AGENT_NICKNAME)
                || is_agent_target_action(role);
            let requires_namespace = reserved
                || role.is_empty()
                || role.chars().all(|ch| ch.is_ascii_digit())
                || role.chars().any(char::is_whitespace)
                || role.chars().any(|ch| matches!(ch, '"' | '\\'))
                || ThreadId::from_string(role).is_ok()
                || ["fork:", "w:", "id:", "ref:", "nick:", "role:"]
                    .iter()
                    .any(|prefix| role.starts_with(prefix));
            let selector = if requires_namespace {
                let quoted = role.replace('\\', "\\\\").replace('"', "\\\"");
                format!("role:\"{quoted}\"")
            } else {
                role.clone()
            };
            AgentPromptTarget {
                thread_id: None,
                selector,
                label: format!("New {role} agent"),
            }
        }));
        self.chat_widget.set_agent_prompt_targets(targets);
    }
}
