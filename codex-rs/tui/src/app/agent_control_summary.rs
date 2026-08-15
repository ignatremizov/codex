//! Canonical thread-state summaries for the `/agent` Overview pane.

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserAgentForkMode;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::protocol::sub_agent_completion_status_from_response_item_id;

use super::ThreadBufferedEvent;
use super::ThreadEventStore;
use super::agent_preview::compact_agent_preview;
use crate::chatwidget::ChatWidget;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct AgentControlSummary {
    pub(super) model: Option<String>,
    pub(super) reasoning_effort: Option<String>,
    pub(super) task_preview: Option<String>,
    pub(super) response_preview: Option<String>,
    pub(super) running_for: Option<Duration>,
    pub(super) terminal_outcome: Option<AgentTerminalOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentTerminalOutcome {
    Completed,
    Interrupted,
    Errored,
}

impl AgentTerminalOutcome {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Errored => "errored",
        }
    }
}

impl AgentControlSummary {
    pub(super) fn from_store(store: &ThreadEventStore) -> Self {
        let model = store
            .session
            .as_ref()
            .map(|session| session.model.trim())
            .filter(|model| !model.is_empty())
            .map(ToOwned::to_owned);
        let reasoning_effort = store
            .session
            .as_ref()
            .and_then(|session| session.reasoning_effort.as_ref())
            .map(ToString::to_string);
        let task_preview = thread_items_newest_first(store).find_map(|item| match item {
            ThreadItem::UserMessage { content, .. } => user_input_preview(content),
            _ => None,
        });
        let response_preview = thread_items_newest_first(store).find_map(|item| match item {
            ThreadItem::AgentMessage { id, text, .. }
                if sub_agent_completion_status_from_response_item_id(id).is_none() =>
            {
                compact_agent_preview(text)
            }
            _ => None,
        });
        let running_for = active_turn_elapsed(store);
        let terminal_outcome = terminal_outcome(store);

        Self {
            model,
            reasoning_effort,
            task_preview,
            response_preview,
            running_for,
            terminal_outcome,
        }
    }
}

pub(super) fn spawned_agent_fork_modes(
    store: &ThreadEventStore,
) -> Vec<(ThreadId, UserAgentForkMode)> {
    thread_items_newest_first(store)
        .filter_map(|item| match item {
            ThreadItem::UserAgentControl {
                action: UserAgentControlAction::Spawn,
                target_thread_id: Some(target_thread_id),
                fork_mode: Some(fork_mode),
                status: UserAgentControlStatus::Succeeded,
                ..
            } => ThreadId::from_string(target_thread_id)
                .ok()
                .map(|target_thread_id| (target_thread_id, *fork_mode)),
            _ => None,
        })
        .collect()
}

fn active_turn_elapsed(store: &ThreadEventStore) -> Option<Duration> {
    let active_turn_id = store.active_turn_id()?;
    let started_at = store
        .buffer
        .iter()
        .rev()
        .find_map(|event| match event {
            ThreadBufferedEvent::Notification(notification) => match notification.as_ref() {
                codex_app_server_protocol::ServerNotification::TurnStarted(notification)
                    if notification.turn.id == active_turn_id =>
                {
                    notification.turn.started_at
                }
                _ => None,
            },
            ThreadBufferedEvent::Request(_)
            | ThreadBufferedEvent::HistoryEntryResponse(_)
            | ThreadBufferedEvent::McpInventoryResult(_)
            | ThreadBufferedEvent::FeedbackSubmission(_) => None,
        })
        .or_else(|| {
            store
                .turns
                .iter()
                .rev()
                .find(|turn| turn.id == active_turn_id)
                .and_then(|turn| turn.started_at)
        });
    started_at.and_then(elapsed_since_unix_seconds).or_else(|| {
        store
            .active_turn_timing()
            .map(|(_, started_at)| started_at.elapsed())
    })
}

fn elapsed_since_unix_seconds(started_at: i64) -> Option<Duration> {
    let started_at =
        UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(started_at).ok()?))?;
    SystemTime::now().duration_since(started_at).ok()
}

fn terminal_outcome(store: &ThreadEventStore) -> Option<AgentTerminalOutcome> {
    if store.active_turn_id().is_some() {
        return None;
    }
    let status = store
        .buffer
        .iter()
        .rev()
        .find_map(|event| match event {
            ThreadBufferedEvent::Notification(notification) => match notification.as_ref() {
                codex_app_server_protocol::ServerNotification::TurnCompleted(notification) => {
                    Some(&notification.turn.status)
                }
                _ => None,
            },
            ThreadBufferedEvent::Request(_)
            | ThreadBufferedEvent::HistoryEntryResponse(_)
            | ThreadBufferedEvent::McpInventoryResult(_)
            | ThreadBufferedEvent::FeedbackSubmission(_) => None,
        })
        .or_else(|| store.turns.last().map(|turn| &turn.status))?;
    match status {
        TurnStatus::Completed => Some(AgentTerminalOutcome::Completed),
        TurnStatus::Interrupted => Some(AgentTerminalOutcome::Interrupted),
        TurnStatus::Failed => Some(AgentTerminalOutcome::Errored),
        TurnStatus::InProgress => None,
    }
}

pub(super) fn agent_fork_mode_label(fork_mode: UserAgentForkMode) -> String {
    match fork_mode {
        UserAgentForkMode::None => "none".to_string(),
        UserAgentForkMode::All => "all".to_string(),
        UserAgentForkMode::LastNTurns { turns } => format!("last {turns} turns"),
    }
}

fn thread_items_newest_first(store: &ThreadEventStore) -> impl Iterator<Item = &ThreadItem> {
    store
        .buffer
        .iter()
        .rev()
        .filter_map(buffered_thread_item)
        .chain(
            store
                .turns
                .iter()
                .rev()
                .flat_map(|turn| turn.items.iter().rev()),
        )
}

fn buffered_thread_item(event: &ThreadBufferedEvent) -> Option<&ThreadItem> {
    match event {
        ThreadBufferedEvent::Notification(notification) => match notification.as_ref() {
            codex_app_server_protocol::ServerNotification::ItemStarted(notification) => {
                Some(&notification.item)
            }
            codex_app_server_protocol::ServerNotification::ItemCompleted(notification) => {
                Some(&notification.item)
            }
            _ => None,
        },
        ThreadBufferedEvent::Request(_)
        | ThreadBufferedEvent::HistoryEntryResponse(_)
        | ThreadBufferedEvent::McpInventoryResult(_)
        | ThreadBufferedEvent::FeedbackSubmission(_) => None,
    }
}

fn user_input_preview(input: &[UserInput]) -> Option<String> {
    let display = ChatWidget::user_message_display_from_inputs(input);
    compact_agent_preview(&display.message).or_else(|| {
        input.iter().find_map(|item| match item {
            UserInput::Image { .. } | UserInput::LocalImage { .. } => Some("[image]".to_string()),
            UserInput::Audio { .. } | UserInput::LocalAudio { .. } => Some("[audio]".to_string()),
            UserInput::Skill { name, .. } => Some(format!("[skill: {name}]")),
            UserInput::Mention { name, .. } => Some(format!("[mention: {name}]")),
            UserInput::Text { .. } => None,
        })
    })
}

#[cfg(test)]
#[path = "agent_control_summary_tests.rs"]
mod tests;
