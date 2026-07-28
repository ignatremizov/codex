//! Fallback terminal delivery, bound before the child's first input is exposed.

use super::*;
use crate::agent::api::AgentTurnOutcome;
use crate::codex_thread::CodexThread;

impl LocalAgentControl {
    pub(super) async fn maybe_start_completion_watcher(
        &self,
        child_thread: &Arc<CodexThread>,
        source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
        version: MultiAgentVersion,
    ) {
        let Some(source) = source else { return };
        let Some(parent_id) = source.parent_thread_id() else {
            return;
        };
        let Ok(manager) = self.runtime.upgrade() else {
            return;
        };
        let Ok(parent) = manager.get_thread(parent_id).await else {
            return;
        };
        let Ok(membership) = self.runtime.admit_start() else {
            return;
        };
        let child = child_thread.session.presentation_id();
        let Some(registration) =
            self.register_completion_watcher_with_parent(child, &parent, &child_reference)
        else {
            return;
        };
        let teardown = membership.into_teardown_guard("completion_watcher", Some(child.thread_id));
        let mut statuses = child_thread.subscribe_status();
        let trace = child_thread.session.services.rollout_thread_trace.clone();
        let control = self.clone();
        tokio::spawn(async move {
            let deliver = |terminal: super::presentation::WatcherTerminalPresentation| {
                let outcome = AgentTurnOutcome {
                    thread_id: child.thread_id,
                    turn_id: terminal.turn_id,
                    source: source.clone(),
                    parent_turn_id: None,
                    initiating_agent_path: None,
                    status: terminal.status,
                    error_info: terminal.presentation.error_info(),
                };
                control.deliver_terminal_completion(
                    outcome,
                    terminal.presentation,
                    &child_reference,
                    (version == MultiAgentVersion::V2)
                        .then(|| child_agent_path.clone())
                        .flatten(),
                    &trace,
                );
            };
            loop {
                while let Some(terminal) = control.take_watcher_terminal_presentation(child) {
                    deliver(terminal);
                }
                let closed = tokio::select! {
                    biased;
                    _ = control.runtime.shutdown.cancelled() => true,
                    result = statuses.changed() => result.is_err(),
                };
                if closed {
                    for terminal in registration.close() {
                        deliver(terminal);
                    }
                    break;
                }
            }
            teardown.complete();
        });
    }
}
