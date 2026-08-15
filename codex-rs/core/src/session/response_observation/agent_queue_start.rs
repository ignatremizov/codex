use codex_protocol::protocol::AgentQueueTurnMetadata;
use tokio::sync::oneshot;

use super::Session;

pub(super) struct AgentQueueTurnStart {
    pub(super) metadata: AgentQueueTurnMetadata,
    pub(super) readiness_sender: Option<oneshot::Sender<()>>,
    pub(super) readiness_receiver: oneshot::Receiver<()>,
}

pub(crate) struct AgentQueueTurnStartPermit {
    pub(super) readiness: oneshot::Sender<()>,
}

impl AgentQueueTurnStartPermit {
    pub(crate) fn publish(self) {
        let _ = self.readiness.send(());
    }
}

impl Session {
    pub(crate) async fn await_agent_queue_turn_metadata(
        &self,
        turn_id: &str,
    ) -> Option<AgentQueueTurnMetadata> {
        let AgentQueueTurnStart {
            mut metadata,
            readiness_receiver,
            ..
        } = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_queue_turns
            .remove(turn_id)?;
        // The target task's tracked startup waits here because only the source-side
        // post-admission path can decide whether handling committed. Forced aborts allow that
        // startup phase to publish queue provenance before terminating the turn.
        if readiness_receiver.await.is_err() {
            metadata.response_handling = None;
        }
        Some(metadata)
    }
}
