use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentQueueTurnMetadata;
use tokio::sync::oneshot;

use super::Session;

#[derive(PartialEq, Eq)]
pub(super) enum ResponseHandlingReadiness {
    Committed,
    Unavailable,
}

pub(super) struct AgentQueueTurnStart {
    pub(super) metadata: AgentQueueTurnMetadata,
    pub(super) readiness_receiver: oneshot::Receiver<ResponseHandlingReadiness>,
}

pub(crate) struct AgentQueueTurnStartPermit {
    pub(super) readiness: oneshot::Sender<ResponseHandlingReadiness>,
}

impl AgentQueueTurnStartPermit {
    pub(crate) fn publish(self) {
        let _ = self.readiness.send(ResponseHandlingReadiness::Committed);
    }

    pub(crate) fn publish_without_response_handling(self) {
        let _ = self.readiness.send(ResponseHandlingReadiness::Unavailable);
    }
}

impl Session {
    pub(crate) fn register_queued_input_start(
        &self,
        submission_id: &str,
        metadata: AgentQueueTurnMetadata,
    ) -> (
        AgentQueueTurnStartPermit,
        oneshot::Receiver<CodexResult<()>>,
    ) {
        let (readiness, readiness_receiver) = oneshot::channel();
        let (receipt, persisted) = oneshot::channel();
        let mut state = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.queued_input_starts.insert(
            submission_id.to_owned(),
            AgentQueueTurnStart {
                metadata,
                readiness_receiver,
            },
        );
        state
            .queued_input_receipts
            .insert(submission_id.to_owned(), receipt);
        (AgentQueueTurnStartPermit { readiness }, persisted)
    }

    pub(crate) fn settle_queued_input_persistence(&self, turn_id: &str, result: CodexResult<()>) {
        if let Some(sender) = self.take_queued_input_persistence(turn_id) {
            let _ = sender.send(result);
        }
    }

    pub(crate) fn take_queued_input_persistence(
        &self,
        turn_id: &str,
    ) -> Option<oneshot::Sender<CodexResult<()>>> {
        self.response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admitted_input_receipts
            .remove(turn_id)
    }

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
        if readiness_receiver.await != Ok(ResponseHandlingReadiness::Committed) {
            metadata.response_handling = None;
        }
        Some(metadata)
    }
}
