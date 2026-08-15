//! Fallback terminal delivery, bound before the child's first input is exposed.

use super::*;
use crate::agent::api::AgentTurnOutcome;
use crate::codex_thread::CodexThread;

impl LocalAgentControl {
    /// Explicitly adopts only future live V1 completions; no cold status is a receipt.
    pub(crate) async fn ensure_native_v1_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        requested_source: SessionSource,
    ) -> CodexResult<CompletionParentAdoption> {
        let parent_id = requested_source.parent_thread_id().ok_or_else(|| {
            CodexErr::InvalidRequest("completion adoption requires a parent".to_string())
        })?;
        let manager = self.upgrade()?;
        let restoration_lock = manager.v2_spawn_resume_lock(child_thread_id);
        let _restoration_guard = restoration_lock.lock_owned().await;
        let child = manager.get_thread(child_thread_id).await?;
        let parent = manager.get_thread(parent_id).await?;
        if Arc::ptr_eq(&child, &parent) {
            return Err(CodexErr::InvalidRequest(
                "an agent cannot adopt itself".to_string(),
            ));
        }
        if !Arc::ptr_eq(&self.state, &parent.session.services.agent_control.state) {
            return Err(CodexErr::InvalidRequest(
                "completion parent belongs to another control".to_string(),
            ));
        }
        parent.session.submission_admission.check_ready()?;
        child.session.submission_admission.check_ready()?;
        let _child_admission = child
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("completion child is closing".to_string()))?;
        if let Some(original_parent_id) = child.session_source.parent_thread_id() {
            let original = manager.get_thread(original_parent_id).await?;
            original.session.submission_admission.check_ready()?;
            let _original_admission = original
                .session
                .submission_admission
                .try_accept_completion_delivery()
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("original completion parent is closing".to_string())
                })?;
            let owner = &child.session.services.agent_control;
            let metadata = owner
                .get_agent_metadata(child_thread_id)
                .unwrap_or_default();
            let reference = metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| child_thread_id.to_string());
            owner.bind_completion_watcher_with_parent(
                &child,
                &original,
                child.session_source.clone(),
                reference,
                metadata.agent_path,
                child.multi_agent_version().unwrap_or(MultiAgentVersion::V1),
            )?;
            if owner
                .completion_parent_for_child(child.session.presentation_id(), original_parent_id)
                != Some(original.session.presentation_id())
            {
                return Err(CodexErr::InvalidRequest(
                    "original completion owner is unavailable".to_string(),
                ));
            }
            return Ok(if Arc::ptr_eq(&original, &parent) {
                CompletionParentAdoption::AlreadyBoundToCaller
            } else {
                CompletionParentAdoption::OriginalParentPreserved
            });
        }
        let (outcome, registration) = child
            .session
            .adopt_v1_completion_parent(self.clone(), &parent)?;
        if let Some(registration) = registration {
            self.start_completion_watcher(
                &child,
                registration,
                child.session_source.clone(),
                child_thread_id.to_string(),
                /*child_agent_path*/ None,
                MultiAgentVersion::V1,
            );
        }
        Ok(outcome)
    }

    #[cfg(test)]
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
        let Ok(manager) = self.upgrade() else { return };
        let Ok(parent) = manager.get_thread(parent_id).await else {
            return;
        };
        if let Err(error) = self.bind_completion_watcher_with_parent(
            child_thread,
            &parent,
            source,
            child_reference,
            child_agent_path,
            version,
        ) {
            warn!("failed to bind completion observer: {error}");
        }
    }

    /// Restoration calls this before publishing the exact runtime in the manager.
    pub(super) fn bind_completion_watcher_with_parent(
        &self,
        child_thread: &Arc<CodexThread>,
        parent: &Arc<CodexThread>,
        source: SessionSource,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
        version: MultiAgentVersion,
    ) -> CodexResult<()> {
        if !Arc::ptr_eq(
            &self.state,
            &child_thread.session.services.agent_control.state,
        ) || !Arc::ptr_eq(&self.state, &parent.session.services.agent_control.state)
        {
            return Err(CodexErr::InvalidRequest(
                "completion child and parent must belong to the binding control".to_string(),
            ));
        }
        if source.parent_thread_id() != Some(parent.session.thread_id()) {
            return Err(CodexErr::InvalidRequest(
                "completion source has another parent".to_string(),
            ));
        }
        let child = child_thread.session.presentation_id();
        let Some(registration) =
            self.register_completion_watcher_with_parent(child, parent, &child_reference)
        else {
            return if self.completion_parent_for_child(child, parent.session.thread_id())
                == Some(parent.session.presentation_id())
            {
                Ok(())
            } else {
                Err(CodexErr::InvalidRequest(
                    "completion parent binding is stale".to_string(),
                ))
            };
        };
        self.start_completion_watcher(
            child_thread,
            registration,
            source,
            child_reference,
            child_agent_path,
            version,
        );
        Ok(())
    }

    fn start_completion_watcher(
        &self,
        child_thread: &Arc<CodexThread>,
        registration: CompletionWatcherRegistration,
        source: SessionSource,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
        version: MultiAgentVersion,
    ) {
        let child = child_thread.session.presentation_id();
        let mut statuses = child_thread.session.subscribe_agent_status_events();
        let trace = child_thread.session.services.rollout_thread_trace.clone();
        let control = self.clone();
        tokio::spawn(async move {
            let _registration = registration;
            loop {
                while let Some(terminal) = control.take_watcher_terminal_presentation(child) {
                    let outcome = AgentTurnOutcome {
                        thread_id: child.thread_id,
                        turn_id: terminal.turn_id,
                        source: source.clone(),
                        parent_turn_id: None,
                        initiating_agent_path: None,
                        status: terminal.status,
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
                }
                if statuses.recv().await.is_none() {
                    return;
                }
            }
        });
    }
}
