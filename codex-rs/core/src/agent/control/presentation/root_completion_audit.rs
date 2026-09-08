//! Model-hidden canonical root oversight, distinct from live-only peer-input copies.
//!
//! A live authored terminal may offer the same immutable evidence to an admitted wait.
//! The publisher acquires its own exact-root delivery capability, then writes only the
//! completion presentation. It installs no model context, observer subscription or grant.

use super::*;
use crate::agent::response_observation::FinalResponseObservation;
use codex_protocol::protocol::MultiAgentVersion;

struct RootAuditPublication {
    root: Arc<CodexThread>,
    finished: bool,
}

impl Drop for RootAuditPublication {
    fn drop(&mut self) {
        if !self.finished {
            self.root.session.quarantine_history(
                "root conclusion publication lost its canonical receipt; do not retry".to_string(),
            );
        }
    }
}

pub(crate) struct PreparedRootCompletionAudit {
    parent: SessionPresentationId,
    child_generation: u64,
}

/// Rechecks this exact audit token under the same transaction as explicit observers.
/// Canonical publication acquires admission first, then this claim, then history.
pub(crate) struct RootCompletionAuditClaim {
    control: LocalAgentControl,
    terminal: AgentTerminalPresentation,
}

impl RootCompletionAuditClaim {
    pub(crate) async fn acquire(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let terminal = &self.terminal.inner;
        let transaction = self
            .control
            .acquire_response_observation_transaction(terminal.parent)
            .await;
        let state = self.control.runtime.wait_agent_presentations.state();
        let key = (terminal.parent, terminal.child, terminal.turn_id.clone());
        let explicit_final = state
            .response_observation_by_observer_child
            .get(&(terminal.parent, terminal.child))
            .and_then(|relationship| relationship.turns.get(&terminal.turn_id))
            .is_some_and(|observation| {
                observation.final_response != FinalResponseObservation::None
            });
        let owns_audit = state.root_audit_turns.contains(&key)
            && state
                .response_terminals
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, terminal));
        drop(state);
        (owns_audit && !explicit_final).then_some(transaction)
    }

    /// Called only after canonical publication succeeds, while the claim is still held.
    pub(crate) fn commit(&self) {
        self.terminal
            .inner
            .ownership
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .committed = true;
        self.terminal.inner.changed.notify_waiters();
        self.control.claim_completion_context_response_item_id(
            self.terminal.inner.parent,
            &self.terminal.inner.context_id,
        );
    }
}

impl LocalAgentControl {
    pub(crate) async fn prepare_root_completion_audit(
        &self,
        child: SessionPresentationId,
        turn_id: &str,
    ) -> codex_protocol::error::Result<Option<PreparedRootCompletionAudit>> {
        let Some(root_id) = self.bound_session_id().map(ThreadId::from) else {
            return Ok(None);
        };
        if root_id == child.thread_id {
            return Ok(None);
        }
        let manager = self.runtime.upgrade()?;
        manager.ensure_native_agent_control()?;
        let root = manager.get_thread(root_id).await?;
        let target = manager.get_thread(child.thread_id).await?;
        if target.session.presentation_id() != child
            || root.multi_agent_version() != Some(MultiAgentVersion::V1)
            || target.multi_agent_version() != Some(MultiAgentVersion::V1)
            || !self
                .runtime
                .shares_tree_with(&root.session.services.local_agent_runtime)
            || !self
                .runtime
                .shares_tree_with(&target.session.services.local_agent_runtime)
        {
            return Ok(None);
        }
        // Do not prepare an audit behind an exact observer's canonical context work.
        // The eventual writer rechecks this policy under the observation transaction.
        if self
            .response_observation_turn_final_response(
                root.session.presentation_id(),
                child,
                turn_id,
            )
            .is_some_and(|policy| policy != FinalResponseObservation::None)
        {
            return Ok(None);
        }
        self.require_current_agent_ownership(child.thread_id)
            .await?;
        Ok(Some(PreparedRootCompletionAudit {
            parent: root.session.presentation_id(),
            child_generation: manager.agent_lifecycle_generation(child.thread_id),
        }))
    }

    /// Called under the child's terminal-publication lock, before terminal status is visible.
    pub(crate) fn record_root_completion_audit(
        &self,
        prepared: PreparedRootCompletionAudit,
        child: SessionPresentationId,
        turn_id: &str,
        status: AgentStatus,
    ) -> Option<AgentTerminalPresentation> {
        if self.runtime.upgrade().is_ok_and(|manager| {
            !manager
                .agent_lifecycle_generation_is_current(child.thread_id, prepared.child_generation)
        }) {
            return None;
        }
        let parent = prepared.parent;
        let mut state = self.runtime.wait_agent_presentations.state();
        let key = (parent, child, turn_id.to_owned());
        let policy = state
            .response_observation_by_observer_child
            .get(&(parent, child))
            .and_then(|relationship| relationship.turns.get(turn_id))
            .map(|observation| observation.final_response);
        if policy.is_some_and(|policy| policy != FinalResponseObservation::None)
            || state.response_terminals.contains_key(&key)
            || state.root_audit_turns.contains(&key)
        {
            return None;
        }
        let reference = self
            .get_agent_metadata(child.thread_id)
            .and_then(|metadata| metadata.agent_path)
            .map_or_else(|| child.thread_id.to_string(), |path| path.to_string());
        let item = codex_protocol::protocol::sub_agent_completion_item(&reference, &status)?;
        let waits = state
            .waits
            .iter()
            .filter_map(|(id, wait)| {
                (wait.parent == parent
                    && wait
                        .children
                        .as_ref()
                        .is_none_or(|children| children.contains(&child.thread_id)))
                .then_some(*id)
            })
            .collect::<HashSet<_>>();
        let sequence = state.next_terminal;
        state.next_terminal = state.next_terminal.saturating_add(1);
        let inner = Arc::new(Terminal {
            parent,
            child,
            turn_id: turn_id.to_owned(),
            sequence,
            parent_thread: Mutex::new(None),
            context_id: new_sub_agent_completion_context_response_item_id(),
            status,
            presentation: CompletionPresentation {
                item: TurnItem::AgentMessage(item),
                history_only_turn_id: Uuid::now_v7().to_string(),
            },
            observation_presentation: OnceLock::new(),
            accepted: Mutex::new(None),
            ownership: Mutex::new(Ownership {
                waits: waits.clone(),
                presenter: None,
                background_claimed: false,
                committed: false,
            }),
            changed: Notify::new(),
        });
        for id in waits {
            if let Some(wait) = state.waits.get_mut(&id) {
                wait.terminals.push(Arc::downgrade(&inner));
            }
        }
        state.root_audit_turns.insert(key.clone());
        state
            .contexts
            .insert(inner.context_id.clone(), Arc::clone(&inner));
        state.response_terminals.insert(key, Arc::clone(&inner));
        let audit = AgentTerminalPresentation { inner };
        let control = self.clone();
        let publication = audit.clone();
        drop(state);
        tokio::spawn(async move {
            publication
                .publish_root_completion_audit(control, prepared.child_generation, reference)
                .await;
        });
        Some(audit)
    }
}

impl AgentTerminalPresentation {
    async fn publish_root_completion_audit(
        self,
        control: LocalAgentControl,
        generation: u64,
        reference: String,
    ) {
        // Never hold a lifecycle or observation lock while waiting for canonical wait commit.
        if self.wait_owns_presentation().await {
            return;
        }
        let Ok(manager) = control.runtime.upgrade() else {
            return;
        };
        let lifecycle = manager
            .v2_spawn_resume_lock(self.inner.child.thread_id)
            .lock_owned()
            .await;
        if !manager.agent_lifecycle_generation_is_current(self.inner.child.thread_id, generation) {
            return;
        }
        let Ok(root) = manager.get_thread(self.inner.parent.thread_id).await else {
            return;
        };
        let Ok(child) = manager.get_thread(self.inner.child.thread_id).await else {
            return;
        };
        if root.session.presentation_id() != self.inner.parent
            || child.session.presentation_id() != self.inner.child
            || root.multi_agent_version() != Some(MultiAgentVersion::V1)
            || child.multi_agent_version() != Some(MultiAgentVersion::V1)
            || !control
                .runtime
                .shares_tree_with(&root.session.services.local_agent_runtime)
            || !control
                .runtime
                .shares_tree_with(&child.session.services.local_agent_runtime)
            || control
                .require_current_agent_ownership(self.inner.child.thread_id)
                .await
                .is_err()
            || root.session.submission_admission.check_ready().is_err()
        {
            return;
        }
        let Some(accepted) = root
            .session
            .submission_admission
            .try_accept_completion_delivery()
        else {
            return;
        };
        drop(lifecycle);
        let Some(presentation) = self.hidden_observation_presentation(&reference) else {
            return;
        };
        let mut publication = RootAuditPublication {
            root: Arc::clone(&root),
            finished: false,
        };
        let claim = RootCompletionAuditClaim {
            control: control.clone(),
            terminal: self.clone(),
        };
        // Reuse the owned canonical writer and stable terminal identity. No context item,
        // communication enqueue, observation snapshot or runtime grant is created.
        match root
            .session
            .publish_root_completion_item(presentation, &accepted, &claim)
            .await
        {
            // Success commits the token before releasing the observer transaction.
            // A superseding observer retains it without a second root presentation.
            Ok(_) => {}
            Err(error) => {
                root.session.quarantine_history(format!(
                    "root conclusion publication outcome unknown: {error}; do not retry"
                ));
                control.claim_completion_context_response_item_id(
                    self.inner.parent,
                    &self.inner.context_id,
                );
            }
        }
        publication.finished = true;
    }
}

#[cfg(test)]
#[path = "root_completion_audit_tests.rs"]
mod tests;
