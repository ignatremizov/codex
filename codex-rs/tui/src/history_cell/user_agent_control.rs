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
    direction_recipient: Option<String>,
}

pub(crate) fn new_user_agent_control(item: ThreadItem) -> Option<UserAgentControlHistoryCell> {
    let ThreadItem::UserAgentControl {
        action,
        authored_selector,
        target_thread_id,
        reply_recipient_thread_id,
        observer_thread_id,
        authored_observer_selector,
        previous_owner_session_id,
        new_owner_session_id,
        agent_ref,
        nickname,
        task_path,
        task,
        task_path_mapping,
        role,
        model,
        reasoning_effort,
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
    if let Some(task_path) = task_path {
        title.push(format!(" {task_path}").cyan());
    }
    if let Some(agent_ref) = agent_ref {
        title.push(format!(" (ref {agent_ref})").dim());
    }
    if let Some(fork_mode) = fork_mode {
        title.push(format!(" ({})", fork_mode_label(fork_mode)).dim());
    }
    let model_and_effort = match (model.as_deref(), reasoning_effort.as_ref()) {
        (Some(model), Some(reasoning_effort)) => Some(format!("{model} {reasoning_effort}")),
        (Some(model), None) => Some(model.to_string()),
        (None, Some(reasoning_effort)) => Some(format!("{reasoning_effort} reasoning")),
        (None, None) => None,
    };
    if let Some(model_and_effort) = model_and_effort {
        title.push(format!(" ({model_and_effort})").dim());
    }
    if let Some(response_observation) = response_observation_label(
        action,
        observe_commentary,
        final_response,
        target_messages,
        queue_input,
    ) && !(action == UserAgentControlAction::ReplyRoute
        && status == UserAgentControlStatus::Succeeded
        && reply_recipient_thread_id.is_some())
    {
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
    if let Some(task) = task {
        audit_details.push(format!("Requested task: {task}").dim().into());
    }
    for mapping in task_path_mapping {
        details.push(
            format!(
                "Task path: {} → {}",
                mapping
                    .previous_task_path
                    .as_deref()
                    .unwrap_or("(unlabeled)"),
                mapping.task_path.as_deref().unwrap_or("(unlabeled)"),
            )
            .dim()
            .into(),
        );
        audit_details.push(
            format!(
                "Task path: {} → {} ({})",
                mapping
                    .previous_task_path
                    .as_deref()
                    .unwrap_or("(unlabeled)"),
                mapping.task_path.as_deref().unwrap_or("(unlabeled)"),
                mapping.thread_id,
            )
            .dim()
            .into(),
        );
    }
    if action == UserAgentControlAction::SubtreeMessaging {
        details.push("Applies to this supervisor and current/future descendants; explicit pair settings take precedence.".dim().into());
    }
    if let Some(selector) = &authored_observer_selector {
        audit_details.push(format!("Observer selector: {selector}").dim().into());
    }
    let direction_recipient = if action == UserAgentControlAction::Observe {
        observer_thread_id.or(authored_observer_selector)
    } else if action == UserAgentControlAction::ReplyRoute
        && status == UserAgentControlStatus::Succeeded
    {
        reply_recipient_thread_id
    } else {
        None
    };
    if let Some(recipient) = &direction_recipient {
        // Keep canonical identity in detail inspection; normal presentation resolves its label.
        let recipient_kind = if action == UserAgentControlAction::Observe {
            "Observer"
        } else {
            "Recipient"
        };
        audit_details.push(format!("{recipient_kind}: {recipient}").dim().into());
        title[1] = if action == UserAgentControlAction::Observe {
            match status {
                UserAgentControlStatus::Succeeded => "User changed observation:".bold(),
                UserAgentControlStatus::Failed => "User observation change failed:".bold(),
            }
        } else if target_messages == Some(true) {
            "User enabled messages:".bold()
        } else {
            "User disabled messages:".bold()
        };
        title.push(" → ".into());
        title.push(recipient.clone().cyan());
    }
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
        direction_recipient,
    })
}

impl UserAgentControlHistoryCell {
    pub(crate) fn with_direction_recipient_label(
        mut self,
        label: impl FnOnce(&str) -> Option<String>,
    ) -> Self {
        if let Some(recipient) = &self.direction_recipient
            && let Some(label) = label(recipient)
            && let Some(span) = self.title.spans.last_mut()
        {
            *span = label.cyan();
        }
        self
    }
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
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::ReplyRoute, _, _) => {
            "User changed reply route for"
        }
        (UserAgentControlStatus::Succeeded, UserAgentControlAction::SubtreeMessaging, _, _) => {
            "User changed subtree messaging for"
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
        (UserAgentControlStatus::Failed, UserAgentControlAction::ReplyRoute, _, _) => {
            "User reply-route change failed for"
        }
        (UserAgentControlStatus::Failed, UserAgentControlAction::SubtreeMessaging, _, _) => {
            "User subtree messaging change failed for"
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
    action: UserAgentControlAction,
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
    if action == UserAgentControlAction::SubtreeMessaging {
        labels.push(if target_messages == Some(true) {
            "enabled"
        } else {
            "disabled"
        });
    } else if target_messages == Some(true) {
        labels.push("allow replies");
    } else if action == UserAgentControlAction::ReplyRoute && target_messages == Some(false) {
        labels.push("no replies");
    }
    if queue_input == Some(true) {
        labels.push("queued turn + reply");
    }
    (!labels.is_empty()).then(|| format!("({})", labels.join(" · ")))
}

#[cfg(test)]
#[path = "user_agent_control_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "user_agent_task_control_tests.rs"]
mod task_tests;
