//! User-facing directional permissions, separate from per-turn response observation.

use super::agent_navigation::AgentNavigationState;
use codex_protocol::ThreadId;
use ratatui::style::Stylize;
use ratatui::text::Line;

pub(super) fn permission_lines(
    navigation: &AgentNavigationState,
    selected: ThreadId,
    root: Option<ThreadId>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(enabled) = navigation.subtree_messaging(selected) {
        lines.push(
            format!(
                "Subtree messaging: {} · includes future agents",
                if enabled { "enabled" } else { "disabled" }
            )
            .bold()
            .into(),
        );
    }
    for (peer, _) in navigation.ordered_threads() {
        if peer == selected {
            continue;
        }
        for (recipient, sender, direction) in [
            (peer, selected, "Send to"),
            (selected, peer, "Receive from"),
        ] {
            let explicit = navigation.reply_route(recipient, sender);
            if let Some(enabled) =
                explicit.or_else(|| navigation.inherited_messaging(sender, recipient))
            {
                let mut label = navigation.display_name(peer, root);
                if let Some(alias) = navigation.alias(peer) {
                    label.push_str(&format!(" (ref {})", alias.agent_ref));
                }
                lines.push(
                    vec![
                        format!("{direction} ").into(),
                        label.cyan(),
                        if enabled {
                            ": enabled".green()
                        } else {
                            ": disabled".dim()
                        },
                        if explicit.is_some() {
                            " · explicit".dim()
                        } else {
                            " · inherited".dim()
                        },
                    ]
                    .into(),
                );
            }
        }
    }
    if !lines.is_empty() {
        lines.insert(0, "Messaging permissions:".bold().into());
        lines.push("Permission does not assign a task.".dim().into());
        lines.push(
            "Visibility labels refer to the viewed thread's model."
                .dim()
                .into(),
        );
    }
    lines
}

#[cfg(test)]
#[path = "agent_messaging_tests.rs"]
mod tests;
