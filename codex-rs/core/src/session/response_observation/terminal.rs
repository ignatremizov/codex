//! Turn-correlated wait observations, separate from native completion ownership.

use super::AgentResponseEvent;
use super::AgentResponseSubscription;
use super::Session;
use codex_protocol::protocol::AgentStatus;

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
                AgentResponseEvent::TurnStarted { turn_id, .. } => (turn_id, AgentStatus::Running),
                AgentResponseEvent::Terminal { turn_id, status } => (turn_id, status),
                AgentResponseEvent::TurnAborted { turn_id } => (turn_id, AgentStatus::Interrupted),
                AgentResponseEvent::Commentary { .. } => continue,
            };
            return Some(TerminalStatusEvent {
                turn_id: Some(turn_id),
                status,
            });
        }
    }
}

impl Session {
    pub(crate) fn subscribe_terminal_status_events(
        &self,
    ) -> (TerminalStatusEvent, TerminalStatusSubscription) {
        let (snapshot, responses) = self.subscribe_agent_responses();
        let turn_id = snapshot.active_turn_id.or_else(|| {
            snapshot
                .last_terminal
                .and_then(|(turn_id, status)| (status == snapshot.status).then_some(turn_id))
        });
        (
            TerminalStatusEvent {
                turn_id,
                status: snapshot.status,
            },
            TerminalStatusSubscription { responses },
        )
    }
}
