//! Explicit, idle-only ownership handoff; unload acknowledgement precedes graph publication.

use super::LoadedSubtree;
use super::ThreadManager;
use super::ThreadManagerState;
use crate::agent::control::AgentTreeMembership;
use codex_agent_graph_store::DetachAgentAliasRequest;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;

pub(crate) enum AgentBranchHandoff {
    DetachFrom(SessionId),
    AdoptRoot,
}

pub(crate) enum AgentDetachAuthority {
    User,
    Agent {
        session_id: SessionId,
        caller: ThreadId,
        _membership: AgentTreeMembership,
    },
}

/// Ownership result after an idle child branch has been durably unloaded and promoted.
pub struct DetachedAgentBranch {
    pub previous_root_thread_id: ThreadId,
    pub unloaded_thread_ids: Vec<ThreadId>,
}

impl ThreadManager {
    /// Promote the selected child and its persisted descendants to an independent root.
    /// Returns the former root; the caller resumes the same UUID under its new ownership.
    pub async fn detach_agent_branch(
        self: &Arc<Self>,
        thread_id: ThreadId,
    ) -> CodexResult<DetachedAgentBranch> {
        self.state
            .detach_agent_branch(thread_id, AgentDetachAuthority::User)
            .await
    }
}

impl ThreadManagerState {
    pub(crate) async fn detach_agent_branch(
        self: &Arc<Self>,
        thread_id: ThreadId,
        authority: AgentDetachAuthority,
    ) -> CodexResult<DetachedAgentBranch> {
        self.ensure_native_agent_control()?;
        let manager = Arc::clone(self);
        // Once accepted, cancellation cannot abandon a partially unloaded ownership operation.
        tokio::spawn(async move {
            let graph = manager
                .agent_graph_store()
                .filter(|graph| graph.supports_agent_aliases())
                .ok_or_else(|| {
                    CodexErr::UnsupportedOperation("detach requires durable native aliases".into())
                })?;
            let owner = manager
                .current_agent_alias(thread_id)
                .await?
                .ok_or_else(|| CodexErr::InvalidRequest("detach requires an owned child".into()))?;
            if owner.session_id == SessionId::from(thread_id) {
                return Err(CodexErr::InvalidRequest(
                    "this conversation is already an independent root".into(),
                ));
            }
            if let AgentDetachAuthority::Agent { session_id, caller, .. } = &authority {
                let source = manager.current_agent_alias(*caller).await?;
                if *caller == thread_id
                    || owner.session_id != *session_id
                    || source.is_none_or(|source| source.session_id != *session_id)
                {
                    return Err(CodexErr::InvalidRequest(
                        "detachment must target another child owned by the caller's current root".into(),
                    ));
                }
                manager.get_thread(*caller).await?.ensure_not_unloading()?;
            }
            if let Ok(root) = manager
                .get_thread(ThreadId::from(owner.session_id))
                .await
            {
                root.ensure_not_unloading()?;
            }
            let branch = manager
                .unload_idle_agent_branch(
                    thread_id,
                    AgentBranchHandoff::DetachFrom(owner.session_id),
                )
                .await?;
            let descendants = graph
                .list_thread_spawn_descendants(thread_id, /*status_filter*/ None)
                .await
                .map_err(|error| CodexErr::Fatal(format!("read detach subtree: {error}")))?;
            let ids = std::iter::once(thread_id)
                .chain(descendants.iter().copied())
                .collect::<Vec<_>>();
            let _writers = manager.reserve_thread_writers(ids.clone()).await?;
            graph
                .detach_agent_alias(DetachAgentAliasRequest {
                    thread_id,
                    expected_previous_session_id: owner.session_id,
                    expected_descendant_thread_ids: descendants,
                })
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!(
                        "branch unloaded but detach could not commit; resume {thread_id} to reload its current owner: {error}"
                    ))
                })?;
            manager.agent_turn_queue.cancel_for_threads(ids);
            let unloaded_thread_ids = branch.thread_ids().to_vec();
            drop(branch);
            drop(authority);
            Ok(DetachedAgentBranch {
                previous_root_thread_id: ThreadId::from(owner.session_id),
                unloaded_thread_ids,
            })
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("detach worker failed: {error}")))?
    }
}

impl ThreadManagerState {
    /// Capture and stop only the requested idle branch, retaining its lifecycle boundaries.
    pub(crate) async fn unload_idle_agent_branch(
        self: &Arc<Self>,
        thread_id: ThreadId,
        handoff: AgentBranchHandoff,
    ) -> CodexResult<LoadedSubtree> {
        self.ensure_native_agent_control()?;
        let manager = ThreadManager {
            state: Arc::clone(self),
            _test_codex_home_guard: None,
        };
        let mut branch = manager.prepare_agent_branch_unload(thread_id).await?;
        let alias = self.current_agent_alias(thread_id).await?.ok_or_else(|| {
            CodexErr::InvalidRequest("ownership handoff requires an alias".into())
        })?;
        let matches = match handoff {
            AgentBranchHandoff::DetachFrom(owner) => {
                alias.session_id == owner && owner != SessionId::from(thread_id)
            }
            AgentBranchHandoff::AdoptRoot => {
                alias.session_id == SessionId::from(thread_id) && alias.agent_ref == 1
            }
        };
        if !matches {
            return Err(CodexErr::InvalidRequest(
                "branch ownership changed before handoff; retry using its current owner".into(),
            ));
        }
        let previous_control = manager
            .retained_native_control_for_session(alias.session_id)
            .await?;
        let all_ids = manager.list_agent_subtree_thread_ids(thread_id).await?;
        // Outbound queue acceptance holds a source gate, even when its target is outside
        // this branch. Fence both endpoints before checking pending work and sealing actors.
        let source_admissions = self
            .agent_turn_queue
            .acquire_source_admissions(all_ids.iter().copied())
            .await;
        if all_ids
            .iter()
            .any(|id| self.agent_turn_queue.has_pending_involving(*id))
        {
            return Err(CodexErr::InvalidRequest(
                "branch has queued agent work; retry the ownership change when idle".into(),
            ));
        }
        let reservations = branch.prepare_idle_handoff().await?;
        drop(source_admissions);
        // Revoke future observations before shutdown can publish a terminal status.
        // Previously accepted exact-instance deliveries retain their independent drain owner.
        for id in branch.thread_ids() {
            self.advance_agent_lifecycle_generation(*id);
        }
        branch.shutdown_and_remove().await?;
        if let Some(control) = previous_control {
            control.forget_unloaded_agent_registrations(all_ids);
        }
        drop(reservations);
        Ok(branch)
    }
}
