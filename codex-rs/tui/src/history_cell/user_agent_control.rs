//! Durable presentation for user-authored `/agent` control actions.

use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserAgentForkMode;
use ratatui::style::Stylize as _;
use ratatui::text::Line;

use super::HistoryCell;
use super::plain_lines;
use crate::multi_agents::format_agent_picker_item_name;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

#[derive(Debug)]
pub(crate) struct UserAgentControlHistoryCell {
    title: Line<'static>,
    details: Vec<Line<'static>>,
    audit_details: Vec<Line<'static>>,
}

pub(crate) fn new_user_agent_control(item: ThreadItem) -> Option<UserAgentControlHistoryCell> {
    let ThreadItem::UserAgentControl {
        action,
        authored_selector,
        target_thread_id,
        previous_owner_session_id,
        new_owner_session_id,
        agent_ref,
        nickname,
        role,
        prompt_preview,
        resumed_target,
        fork_mode,
        observe_commentary,
        final_response,
        target_messages,
        queue_input,
        status,
        error,
        ..
    } = item
    else {
        return None;
    };

    let target = control_target_label(
        authored_selector.as_deref(),
        target_thread_id.as_deref(),
        agent_ref.as_deref(),
        nickname.as_deref(),
        role.as_deref(),
    );
    let mut title = vec![
        "• ".dim(),
        control_action_title(
            action,
            status,
            prompt_preview.is_some(),
            new_owner_session_id.is_some(),
            resumed_target,
        )
        .bold(),
    ];
    if let Some(target) = target {
        title.push(" ".into());
        title.push(target.cyan());
    }
    if let Some(agent_ref) = agent_ref {
        title.push(format!(" (ref {agent_ref})").dim());
    }
    if let Some(fork_mode) = fork_mode {
        title.push(format!(" ({})", fork_mode_label(fork_mode)).dim());
    }
    if let Some(response_observation) = response_observation_label(
        observe_commentary,
        final_response,
        target_messages,
        queue_input,
    ) {
        title.push(" ".into());
        title.push(
            if observe_commentary == Some(true)
                || matches!(final_response, Some(AgentFinalResponseHandling::Wake))
                || target_messages == Some(true)
                || queue_input == Some(true)
            {
                response_observation.magenta()
            } else {
                response_observation.dim()
            },
        );
    }

    let mut details = Vec::new();
    if let Some(prompt_preview) = prompt_preview
        && !prompt_preview.is_empty()
    {
        details.push(prompt_preview.into());
    }
    if let Some(error) = error
        && !error.is_empty()
    {
        details.push(if status == UserAgentControlStatus::Failed {
            vec!["Failed: ".red(), error.red()].into()
        } else {
            vec!["Warning: ".magenta(), error.magenta()].into()
        });
    }

    let mut audit_details = Vec::new();
    if let Some(target_thread_id) = target_thread_id {
        audit_details.push(format!("Target: {target_thread_id}").dim().into());
    }
    if let Some(authored_selector) = authored_selector {
        audit_details.push(format!("Selector: {authored_selector}").dim().into());
    }
    if let Some(new_owner_session_id) = new_owner_session_id {
        let previous_owner = previous_owner_session_id.as_deref().unwrap_or("unowned");
        audit_details.push(
            format!("Ownership: {previous_owner} → {new_owner_session_id}")
                .dim()
                .into(),
        );
    }

    Some(UserAgentControlHistoryCell {
        title: title.into(),
        details,
        audit_details,
    })
}

impl HistoryCell for UserAgentControlHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![self.title.clone()];
        for (index, detail) in self.details.iter().enumerate() {
            let opts = RtOptions::new(width.max(1) as usize)
                .initial_indent(if index == 0 {
                    "  └ ".dim().into()
                } else {
                    "    ".into()
                })
                .subsequent_indent("    ".into());
            lines.extend(word_wrap_lines([detail.clone()], opts));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![self.title.clone()];
        lines.extend(self.details.clone());
        lines.extend(self.audit_details.clone());
        plain_lines(lines)
    }
}

fn control_action_title(
    action: UserAgentControlAction,
    status: UserAgentControlStatus,
    has_prompt: bool,
    is_adoption: bool,
    resumed_target: bool,
) -> &'static str {
    if action == UserAgentControlAction::Resume && is_adoption {
        return match status {
            UserAgentControlStatus::Succeeded => "User adopted",
            UserAgentControlStatus::Failed => "User agent adoption failed for",
        };
    }
    match (status, action, has_prompt, resumed_target) {
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Prompt, _, true) => {
            "User resumed and sent to"
        }
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::QueuedPrompt, _, true) => {
            "User resumed and sent queued prompt to"
        }
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Spawn, _, _) => "User spawned",
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Prompt, _, _) => "User sent to",
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::QueuedPrompt, _, _) => {
            "User sent queued prompt to"
        }
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Resume, _, _) => "User resumed",
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Interrupt, true, _) => {
            "User interrupted and sent to"
        }
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Interrupt, false, _) => {
            "User interrupted"
        }
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Close, _, _) => "User closed",
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::Observe, _, _) => {
            "User changed observation for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::Spawn, _, _) => {
            "User agent spawn failed"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::Prompt, _, _) => {
            "User agent prompt failed for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::QueuedPrompt, _, _) => {
            "User queued agent prompt failed for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::Resume, _, _) => {
            "User agent resume failed for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::Interrupt, _, _) => {
            "User agent interrupt failed for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::Close, _, _) => {
            "User agent close failed for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::Observe, _, _) => {
            "User observation change failed for"
        }
    }
}

fn control_target_label(
    authored_selector: Option<&str>,
    target_thread_id: Option<&str>,
    agent_ref: Option<&str>,
    nickname: Option<&str>,
    role: Option<&str>,
) -> Option<String> {
    if agent_ref == Some("1") {
        return Some(format_agent_picker_item_name(
            /*agent_nickname*/ None, /*agent_role*/ None, /*is_primary*/ true,
        ));
    }
    if nickname.is_some() || role.is_some() {
        return Some(format_agent_picker_item_name(
            nickname, role, /*is_primary*/ false,
        ));
    }
    authored_selector
        .filter(|selector| !selector.is_empty())
        .or(target_thread_id.filter(|thread_id| !thread_id.is_empty()))
        .map(str::to_string)
}

fn fork_mode_label(fork_mode: UserAgentForkMode) -> String {
    match fork_mode {
        UserAgentForkMode::None => "fork none".to_string(),
        UserAgentForkMode::All => "fork all".to_string(),
        UserAgentForkMode::LastNTurns { turns } => format!("fork last {turns}"),
    }
}

fn response_observation_label(
    observe_commentary: Option<bool>,
    final_response: Option<AgentFinalResponseHandling>,
    target_messages: Option<bool>,
    queue_input: Option<bool>,
) -> Option<String> {
    let mut labels = Vec::new();
    if observe_commentary == Some(true) {
        labels.push("commentary");
    }
    match final_response {
        Some(AgentFinalResponseHandling::None) => labels.push("ignore final reply"),
        Some(AgentFinalResponseHandling::Passive) => labels.push("passive"),
        Some(AgentFinalResponseHandling::Wake) => labels.push("wake"),
        Some(AgentFinalResponseHandling::Presentation) => labels.push("presentation"),
        None => {}
    }
    if target_messages == Some(true) {
        labels.push("allow replies");
    }
    if queue_input == Some(true) {
        labels.push("queued turn + reply");
    }
    (!labels.is_empty()).then(|| format!("({})", labels.join(" · ")))
}

#[cfg(test)]
#[path = "user_agent_control_tests.rs"]
mod tests;
