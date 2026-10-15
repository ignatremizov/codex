//! Local replacement-server recovery is deliberately split into preparation and admission.
//! Preparation attaches unfinished agents and their required owners without loading idle history.
//! Only durable, unfinished, receiver-owned regular turns are eligible for admission.

use super::ThreadRequestProcessor;
use super::ThreadResumeTarget;
use crate::outgoing_message::ConnectionId;
use codex_app_server_protocol::ThreadEnvironment;
use codex_app_server_protocol::ThreadResumeParams;
use codex_core::StartIfIdleSubmission;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::turn_input::RecoverTurnRequest;
use codex_state::AgentAliasState;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::Weak;

#[path = "local_tree_recovery_settings.rs"]
mod settings;
use settings::validate_recovery_settings;
#[path = "local_tree_recovery_history.rs"]
mod history;

/// A single-use recovery plan bound to the replacement server that prepared it.
///
/// Dropping this plan leaves restored threads idle. It does not submit continuation input.
pub struct PreparedLocalTreeRecovery {
    manager: Weak<ThreadManager>,
    root_thread_id: ThreadId,
    thread_ids: Vec<ThreadId>,
    warnings: Vec<String>,
    candidates: Vec<(ThreadId, RecoveryCandidate)>,
}

impl PreparedLocalTreeRecovery {
    pub fn root_thread_id(&self) -> ThreadId {
        self.root_thread_id
    }

    pub fn thread_ids(&self) -> &[ThreadId] {
        &self.thread_ids
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Per-thread admission results; a paused child does not invalidate the usable session.
#[derive(Debug, Default)]
pub struct LocalTreeRecoveryOutcome {
    pub resumed_turns: Vec<ThreadId>,
    pub paused_threads: Vec<(ThreadId, String)>,
}

struct RecoveryCandidate {
    context: TurnContextItem,
    fingerprint: serde_json::Value,
}

impl ThreadRequestProcessor {
    pub(crate) async fn prepare_local_tree_recovery(
        &self,
        selected: ThreadId,
        connection_id: ConnectionId,
    ) -> io::Result<PreparedLocalTreeRecovery> {
        let db = self.state_db.as_ref().ok_or_else(|| {
            io::Error::other("agent-tree recovery requires persisted ownership state")
        })?;
        let mut root = selected;
        let mut ancestors = HashSet::from([selected]);
        while let Some(parent) = db
            .find_thread_spawn_parent(root)
            .await
            .map_err(io::Error::other)?
        {
            let open = db
                .list_thread_spawn_children_with_status(
                    parent,
                    DirectionalThreadSpawnEdgeStatus::Open,
                )
                .await
                .map_err(io::Error::other)?;
            if !open.contains(&root) {
                return Err(io::Error::other(
                    "selected thread has a closed ownership edge",
                ));
            }
            if !ancestors.insert(parent) || ancestors.len() > 1024 {
                return Err(io::Error::other(
                    "invalid or oversized agent ownership chain",
                ));
            }
            root = parent;
        }
        let descendants = db
            .list_thread_spawn_descendants_with_status(root, DirectionalThreadSpawnEdgeStatus::Open)
            .await
            .map_err(io::Error::other)?;
        if descendants.len() > 1024 {
            return Err(io::Error::other("agent tree exceeds local recovery limit"));
        }
        let mut plan = PreparedLocalTreeRecovery {
            manager: Arc::downgrade(&self.thread_manager),
            root_thread_id: root,
            thread_ids: Vec::new(),
            warnings: Vec::new(),
            candidates: Vec::new(),
        };
        // An open ownership edge survives unloading and can outlive the agent's work.
        // Inspect durable work before spending a runtime slot on historical descendants.
        let descendant_ids: HashSet<_> = descendants.iter().copied().collect();
        let mut needed = ancestors.clone();
        for &thread_id in &descendants {
            if needed.contains(&thread_id) {
                continue;
            }
            let branch = async {
                if self.local_recovery_history(thread_id).await?.is_none() {
                    return Ok(HashSet::new());
                }
                let mut branch = HashSet::new();
                let mut current = thread_id;
                while !needed.contains(&current) {
                    if !descendant_ids.contains(&current) || !branch.insert(current) {
                        return Err(io::Error::other(
                            "unfinished agent has an invalid ownership chain",
                        ));
                    }
                    current = db
                        .find_thread_spawn_parent(current)
                        .await
                        .map_err(io::Error::other)?
                        .ok_or_else(|| io::Error::other("unfinished agent has no owning parent"))?;
                }
                Ok::<_, io::Error>(branch)
            }
            .await;
            match branch {
                Ok(branch) => needed.extend(branch),
                Err(err) => plan
                    .warnings
                    .push(format!("Agent {thread_id} remains paused: {err}")),
            }
        }
        let mut prepared = HashSet::new();
        for thread_id in std::iter::once(root)
            .chain(descendants)
            .filter(|id| needed.contains(id))
        {
            let result = async {
                if let Some(alias) = db
                    .find_current_agent_alias_by_thread(thread_id)
                    .await
                    .map_err(io::Error::other)?
                    && (alias.state != AgentAliasState::Active || alias.session_id != root.into())
                {
                    return Err(io::Error::other(
                        "thread belongs to another or closed agent tree",
                    ));
                }
                if thread_id != root {
                    let parent = db
                        .find_thread_spawn_parent(thread_id)
                        .await
                        .map_err(io::Error::other)?;
                    if !parent.is_some_and(|parent| prepared.contains(&parent)) {
                        return Err(io::Error::other("owning parent could not be restored"));
                    }
                }
                self.thread_resume(
                    ThreadResumeTarget::LocalTreeRecovery(connection_id),
                    ThreadResumeParams {
                        thread_id: thread_id.to_string(),
                        exclude_turns: true,
                        ..Default::default()
                    },
                    /*app_server_client_name*/ None,
                    /*app_server_client_version*/ None,
                    Default::default(),
                )
                .await
                .map_err(|err| io::Error::other(err.message))?;
                Ok::<(), io::Error>(())
            }
            .await;
            if let Err(err) = result {
                if ancestors.contains(&thread_id) {
                    return Err(io::Error::other(format!(
                        "cannot restore required thread {thread_id}: {err}"
                    )));
                }
                plan.warnings
                    .push(format!("Agent {thread_id} was not restored: {err}"));
                continue;
            }
            prepared.insert(thread_id);
            plan.thread_ids.push(thread_id);
            match self.local_recovery_candidate(thread_id).await {
                Ok(Some(candidate)) => plan.candidates.push((thread_id, candidate)),
                Ok(None) => {}
                Err(err) => plan
                    .warnings
                    .push(format!("Agent {thread_id} remains paused: {err}")),
            }
        }
        if !prepared.contains(&selected) {
            return Err(io::Error::other(
                "selected thread is not in the active ownership tree",
            ));
        }
        Ok(plan)
    }

    pub(crate) async fn continue_local_tree_recovery(
        &self,
        plan: PreparedLocalTreeRecovery,
    ) -> io::Result<LocalTreeRecoveryOutcome> {
        if !Weak::ptr_eq(&plan.manager, &Arc::downgrade(&self.thread_manager)) {
            return Err(io::Error::other("recovery plan belongs to another server"));
        }
        let mut outcome = LocalTreeRecoveryOutcome::default();
        for (thread_id, expected) in plan.candidates {
            let result = async {
                let db = self
                    .state_db
                    .as_ref()
                    .ok_or_else(|| io::Error::other("ownership state unavailable"))?;
                if let Some(alias) = db
                    .find_current_agent_alias_by_thread(thread_id)
                    .await
                    .map_err(io::Error::other)?
                    && (alias.state != AgentAliasState::Active
                        || alias.session_id != plan.root_thread_id.into())
                {
                    return Err(io::Error::other(
                        "agent ownership changed after preparation",
                    ));
                }
                if thread_id != plan.root_thread_id
                    && !db
                        .list_thread_spawn_descendants_with_status(
                            plan.root_thread_id,
                            DirectionalThreadSpawnEdgeStatus::Open,
                        )
                        .await
                        .map_err(io::Error::other)?
                        .contains(&thread_id)
                {
                    return Err(io::Error::other("agent is no longer an open descendant"));
                }
                let thread = self
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .map_err(io::Error::other)?;
                let current = self
                    .local_recovery_candidate(thread_id)
                    .await?
                    .ok_or_else(|| io::Error::other("turn completed or no longer eligible"))?;
                if current.fingerprint != expected.fingerprint {
                    return Err(io::Error::other(
                        "durable work or runtime settings changed after preparation",
                    ));
                }
                let turn_id = current
                    .context
                    .turn_id
                    .ok_or_else(|| io::Error::other("missing turn identity"))?;
                match thread
                    .recover_turn_if_still_current(RecoverTurnRequest {
                        turn_id,
                        thread_settings: Default::default(),
                        trace: None,
                        cyber_access_program: current.context.cyber_access_program,
                    })
                    .await
                    .map_err(io::Error::other)?
                {
                    StartIfIdleSubmission::Started { .. } => Ok(()),
                    StartIfIdleSubmission::NotSubmitted { reason } => {
                        Err(io::Error::other(format!("turn not admitted: {reason:?}")))
                    }
                }
            }
            .await;
            match result {
                Ok(()) => outcome.resumed_turns.push(thread_id),
                Err(err) => outcome.paused_threads.push((thread_id, err.to_string())),
            }
        }
        Ok(outcome)
    }

    async fn local_recovery_candidate(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<Option<RecoveryCandidate>> {
        // Re-read after loading and again before admission: selection is not authorization
        // to continue a turn whose durable work or settings changed during preparation.
        let Some(history::RecoveryHistory {
            context,
            own_segment,
        }) = self.local_recovery_history(thread_id).await?
        else {
            return Ok(None);
        };
        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(io::Error::other)?;
        let config = thread.config_snapshot().await;
        validate_recovery_settings(&context, &config)?;
        let fingerprint = serde_json::json!({
            "history": own_segment,
            "context": context,
            "environments": config.environment_selections().iter().map(ThreadEnvironment::from).collect::<Vec<_>>(),
            "permissions": config.permission_profile,
            "approvalPolicy": config.approval_policy,
            "approvalsReviewer": config.approvals_reviewer,
            "workspaceRoots": config.workspace_roots,
            "model": config.model,
            "collaborationMode": config.collaboration_mode,
            "reasoningEffort": config.reasoning_effort,
            "reasoningSummary": config.reasoning_summary,
            "personality": config.personality,
            "disabledPluginIds": config.disabled_plugin_ids,
            "serviceTier": config.service_tier,
        });
        Ok(Some(RecoveryCandidate {
            context,
            fingerprint,
        }))
    }
}

#[cfg(test)]
#[path = "local_tree_recovery_tests.rs"]
mod tests;
