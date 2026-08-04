//! Turn-correlated wait observations, separate from native completion ownership.

use super::AgentResponseEvent;
use super::AgentResponseSubscription;
use super::Session;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalStatusEvent {
    pub(crate) turn_id: Option<String>,
    pub(crate) status: AgentStatus,
}

pub(crate) struct TerminalStatusSubscription {
    responses: AgentResponseSubscription,
}

impl TerminalStatusSubscription {
    pub(crate) async fn recv(&mut self) -> Option<TerminalStatusEvent> {
        loop {
            let (turn_id, status) = match self.responses.recv().await? {
                AgentResponseEvent::Terminal { turn_id, status } => (turn_id, status),
                AgentResponseEvent::TurnAborted { turn_id } => (turn_id, AgentStatus::Interrupted),
                AgentResponseEvent::TurnStarted { .. } | AgentResponseEvent::Commentary { .. } => {
                    continue;
                }
            };
            return Some(TerminalStatusEvent {
                turn_id: Some(turn_id),
                status,
            });
        }
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;

impl Session {
    pub(crate) fn subscribe_terminal_status_events(
        &self,
    ) -> (TerminalStatusEvent, TerminalStatusSubscription) {
        let (snapshot, responses) = self.subscribe_agent_responses();
        let terminal = snapshot.last_terminal.filter(|(turn_id, outcome)| {
            snapshot.active_turn_id.is_none()
                && snapshot.latest_admitted_turn_id.as_ref() == Some(turn_id)
                && (outcome.status == snapshot.status
                    || (snapshot.status == AgentStatus::Interrupted
                        && outcome.error_info == Some(CodexErrorInfo::TooManyDenials)))
        });
        let (turn_id, status) = terminal.map_or(
            (snapshot.active_turn_id, snapshot.status),
            |(turn_id, outcome)| (Some(turn_id), outcome.status),
        );
        (
            TerminalStatusEvent { turn_id, status },
            TerminalStatusSubscription { responses },
        )
    }
}
