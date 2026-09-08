//! Root oversight of live V1 terminal turns that have no root-owned final observation.
//!
//! Audit uses ordinary terminal/wait arbitration and durable, model-hidden completion
//! presentation. It never installs a response observer or restores historical grants.

use super::*;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SubAgentCompletionModelVisibility;

/// Keeps explicit observation binding ordered until the child's terminal-status check.
pub(crate) struct PreparedRootCompletionAudit {
    pub(crate) parent: SessionPresentationId,
    pub(crate) admission: AcceptedCompletionDelivery,
    pub(crate) observation: OwnedMutexGuard<()>,
}

impl AgentControl {
    pub(crate) async fn prepare_root_completion_audit(
        &self,
        child: SessionPresentationId,
    ) -> codex_protocol::error::Result<Option<PreparedRootCompletionAudit>> {
        let Some(root_id) = self.bound_session_id().map(ThreadId::from) else {
            return Ok(None);
        };
        if root_id == child.thread_id {
            return Ok(None);
        }
        let manager = self.upgrade()?;
        let root = manager.get_thread_including_pending(root_id).await?;
        let target = manager
            .get_thread_including_pending(child.thread_id)
            .await?;
        if target.session.presentation_id() != child
            || root.multi_agent_version() != Some(MultiAgentVersion::V1)
            || target.multi_agent_version() != Some(MultiAgentVersion::V1)
        {
            return Ok(None);
        }
        self.require_current_agent_ownership(child.thread_id)
            .await?;
        let Some(admission) = root
            .session
            .submission_admission
            .try_accept_completion_delivery()
        else {
            return Ok(None);
        };
        let parent = root.session.presentation_id();
        // Observation admission and audit registration must agree on the exact target
        // turn. The shared presentation lock checks policy and reserves the token atomically.
        let observation = self.acquire_response_observation_transaction(parent).await;
        Ok(Some(PreparedRootCompletionAudit {
            parent,
            admission,
            observation,
        }))
    }
}

impl AgentTerminalPresentation {
    pub(crate) fn publish_root_completion_audit(self, control: AgentControl) {
        // Wait ownership may settle only after the target has finished publishing its
        // terminal event; do not block that publication on the waiting parent.
        tokio::spawn(async move {
            let Ok(manager) = control.upgrade() else {
                return;
            };
            let generation = manager.agent_lifecycle_generation(self.inner.child.thread_id);
            let Some(_lifecycle) = control
                .acquire_current_agent_lifecycle(self.inner.child.thread_id, generation)
                .await
            else {
                return;
            };
            let Ok(root) = manager
                .get_thread_including_pending(self.inner.parent.thread_id)
                .await
            else {
                return;
            };
            let Ok(child) = manager
                .get_thread_including_pending(self.inner.child.thread_id)
                .await
            else {
                return;
            };
            if root.session.presentation_id() != self.inner.parent
                || child.session.presentation_id() != self.inner.child
                || root.multi_agent_version() != Some(MultiAgentVersion::V1)
                || child.multi_agent_version() != Some(MultiAgentVersion::V1)
                || control.bound_session_id().map(ThreadId::from)
                    != Some(self.inner.parent.thread_id)
                || control
                    .require_current_agent_ownership(self.inner.child.thread_id)
                    .await
                    .is_err()
            {
                return;
            }
            // Match watcher delivery's lock order: target lifecycle, destination
            // mailbox, then observation transaction. Oversight never submits input.
            let Ok(_mailbox) = control
                .acquire_mailbox_submission_permit(self.inner.parent.thread_id)
                .await
            else {
                return;
            };
            let _observation = control
                .acquire_response_observation_transaction(self.inner.parent)
                .await;
            let explicitly_observed = control
                .response_observation_relationship_snapshot(self.inner.parent, self.inner.child)
                .is_some_and(|relationship| relationship.turns.contains_key(&self.inner.turn_id));
            if explicitly_observed {
                // A root observation can bind between terminal preparation and this
                // worker. Hand the same token to its watcher instead of competing.
                control.requeue_watcher_terminal_presentation(
                    self.inner.parent,
                    self.inner.child,
                    WatcherTerminalPresentation {
                        turn_id: self.inner.turn_id.clone(),
                        status: self.inner.status.clone(),
                        presentation: self,
                    },
                );
                control
                    .wait_agent_presentations
                    .watcher_terminal_changed
                    .notify_waiters();
                return;
            }
            if self.wait_owns_presentation().await {
                return;
            }
            let reference = control
                .get_agent_metadata(self.inner.child.thread_id)
                .and_then(|metadata| metadata.agent_path)
                .map_or_else(
                    || self.inner.child.thread_id.to_string(),
                    |path| path.to_string(),
                );
            if let Some(admission) = self.take_accepted_completion_delivery().or_else(|| {
                root.session
                    .submission_admission
                    .try_accept_completion_delivery()
            }) {
                root.emit_accepted_sub_agent_completion_without_turn(
                    &reference,
                    &self.inner.status,
                    SubAgentCompletionModelVisibility::NotVisible,
                    admission,
                )
                .await;
            }
        });
    }
}

#[cfg(test)]
#[path = "root_completion_audit_tests.rs"]
mod tests;
