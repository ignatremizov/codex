//! Ordered submission attempts across public interrupts and trusted agent input.
//!
//! Agent input no longer travels through `Op`. Keep these fixture observations
//! separate from the public-op log so ordering and duplicate-admission checks
//! still cover both submission paths without restoring a public trusted-input API.

use super::ThreadManager;
use super::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentInputPresentation;

#[derive(Clone, Debug)]
pub(crate) enum CapturedAgentOperation {
    Interrupt,
    AgentInput {
        presentation: AgentInputPresentation,
    },
}

impl ThreadManager {
    pub(crate) fn captured_agent_operations(&self) -> Vec<(ThreadId, CapturedAgentOperation)> {
        self.state
            .agent_operations
            .lock()
            .expect("agent operation capture")
            .clone()
    }
}

impl ThreadManagerState {
    pub(crate) fn capture_agent_operation(
        &self,
        thread_id: ThreadId,
        operation: CapturedAgentOperation,
    ) {
        if self.ops_log.is_none() {
            return;
        }
        self.agent_operations
            .lock()
            .expect("agent operation capture")
            .push((thread_id, operation));
    }
}
