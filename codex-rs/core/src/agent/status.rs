use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use serde::Serialize;

/// Version-neutral roster state: discovery is not a delivery of a final answer or error body.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactAgentStatus {
    PendingInit,
    Running,
    Completed,
    Interrupted,
    Errored,
    Closed,
    Unloaded,
}

impl CompactAgentStatus {
    pub(crate) const ALL: [Self; 7] = [
        Self::PendingInit,
        Self::Running,
        Self::Completed,
        Self::Interrupted,
        Self::Errored,
        Self::Closed,
        Self::Unloaded,
    ];
}

impl From<&AgentStatus> for CompactAgentStatus {
    fn from(status: &AgentStatus) -> Self {
        match status {
            AgentStatus::PendingInit => Self::PendingInit,
            AgentStatus::Running => Self::Running,
            AgentStatus::Completed(_) => Self::Completed,
            AgentStatus::Interrupted => Self::Interrupted,
            AgentStatus::Errored(_) => Self::Errored,
            AgentStatus::Shutdown => Self::Closed,
            AgentStatus::NotFound => Self::Unloaded,
        }
    }
}

/// Derive the next agent status from a single emitted event.
/// Returns `None` when the event does not affect status tracking.
pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
        EventMsg::TurnComplete(ev) => Some(match &ev.error {
            Some(error) => AgentStatus::Errored(error.message.clone()),
            None => AgentStatus::Completed(ev.last_agent_message.clone()),
        }),
        EventMsg::TurnAborted(ev) => match ev.reason {
            codex_protocol::protocol::TurnAbortReason::Interrupted
            | codex_protocol::protocol::TurnAbortReason::BudgetLimited => {
                Some(AgentStatus::Interrupted)
            }
            _ => Some(AgentStatus::Errored(format!("{:?}", ev.reason))),
        },
        EventMsg::Error(ev) if ev.affects_turn_status() => {
            Some(AgentStatus::Errored(ev.message.clone()))
        }
        EventMsg::Error(_) => None,
        EventMsg::ShutdownComplete => Some(AgentStatus::Shutdown),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    !matches!(
        status,
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
    )
}
