use super::*;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::mcp::ClientMcpExtensions;

struct PersistedV2SpawnResume {
    child_thread_id: ThreadId,
    parent_thread_id: ThreadId,
    session_id: SessionId,
    session_source: SessionSource,
    edge_status: ThreadSpawnEdgeStatus,
}

impl ThreadManager {
    /// Reloads a recorded Multi-Agent V2 child through its currently loaded immediate parent.
    ///
    /// The child keeps the existing parent-controlled reload semantics. Callers cannot supply
    /// configuration overrides, and an unavailable or unrecognized owner is an error.
    pub async fn ensure_multi_agent_v2_child_loaded(
        &self,
        child_thread_id: ThreadId,
    ) -> CodexResult<()> {
        let stored_thread = self
            .state
            .read_stored_thread(ReadThreadParams {
                thread_id: child_thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let history = match stored_thread.history_mode {
            ThreadHistoryMode::Legacy => {
                self.state
                    .read_stored_thread(ReadThreadParams {
                        thread_id: child_thread_id,
                        include_archived: true,
                        include_history: true,
                    })
                    .await?
                    .history
                    .ok_or(CodexErr::ThreadNotFound(child_thread_id))?
                    .items
            }
            ThreadHistoryMode::Paginated => {
                self.state
                    .load_latest_model_context(LoadThreadHistoryParams {
                        thread_id: child_thread_id,
                        include_archived: true,
                    })
                    .await?
                    .items
            }
        };
        let history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: child_thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        let Some(resume) = self.state.persisted_v2_spawn_resume(&history).await? else {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {child_thread_id} is not a recorded multi-agent v2 child"
            )));
        };
        let parent_thread_id = resume.parent_thread_id;
        let parent = self.get_thread(parent_thread_id).await.map_err(|_| {
            CodexErr::InvalidRequest(format!(
                "cannot resume multi-agent v2 child {child_thread_id}: parent {parent_thread_id} is not loaded; resume the parent first"
            ))
        })?;
        if parent.session.services.agent_control.session_id() != resume.session_id {
            return Err(CodexErr::InvalidRequest(
                "recorded child belongs to a different owning control session".to_string(),
            ));
        }
        let config = parent.session.get_config().await.as_ref().clone();
        let agent_control = parent.session.services.agent_control.clone();
        agent_control
            .ensure_v2_agent_loaded(config, child_thread_id)
            .await
    }

    pub(super) async fn try_resume_persisted_v2_spawn(
        &self,
        config: &Config,
        initial_history: &InitialHistory,
        client_mcp_extensions: &ClientMcpExtensions,
    ) -> CodexResult<Option<NewThread>> {
        let state = Arc::clone(&self.state);
        let config = config.clone();
        let history = initial_history.clone();
        let extensions = client_mcp_extensions.clone();
        tokio::spawn(async move {
            state
                .try_resume_persisted_v2_spawn(&config, &history, &extensions)
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("restoration worker failed: {error}")))?
    }
}

impl ThreadManagerState {
    /// Resume a persisted V2 spawned child through its live owning control plane.
    ///
    /// A graph-backed child must not fall through to generic cold resume once its persisted
    /// parent/child edge has been recognized. Doing so would construct a detached `AgentControl`
    /// and leave path routing, completion delivery, and residency accounting split across two
    /// control planes. When the direct parent or its matching control identity is unavailable,
    /// return a recoverable error so callers can resume the owner chain first.
    pub(super) async fn try_resume_persisted_v2_spawn(
        self: &Arc<Self>,
        config: &Config,
        initial_history: &InitialHistory,
        client_mcp_extensions: &ClientMcpExtensions,
    ) -> CodexResult<Option<NewThread>> {
        let Some(initial_resume) = self.persisted_v2_spawn_resume(initial_history).await? else {
            return Ok(None);
        };
        let resume_lock = self.v2_spawn_resume_lock(initial_resume.child_thread_id);
        let _resume_guard = resume_lock.lock_owned().await;
        self.check_restoration_fence(initial_resume.child_thread_id)?;
        let Some(resume) = self.persisted_v2_spawn_resume(initial_history).await? else {
            return Ok(None);
        };

        let parent_thread = self
            .get_thread(resume.parent_thread_id)
            .await
            .map_err(|_| {
                CodexErr::InvalidRequest(format!(
                    "cannot resume spawned V2 child {} while its direct parent {} is not loaded; resume the parent or owning root chain first",
                    resume.child_thread_id, resume.parent_thread_id
                ))
            })?;
        if parent_thread.multi_agent_version() != Some(MultiAgentVersion::V2) {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} through parent {} because the parent is not running Multi-Agent V2",
                resume.child_thread_id, resume.parent_thread_id
            )));
        }

        let owner = parent_thread.session.services.agent_control.clone();
        let expected_parent_path = parent_thread
            .session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let owner_has_parent_identity = owner
            .get_agent_metadata(resume.parent_thread_id)
            .is_some_and(|metadata| metadata.agent_path.as_ref() == Some(&expected_parent_path));
        if owner.session_id() != resume.session_id || !owner_has_parent_identity {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} because the loaded parent {} is not attached to its persisted owning control plane; resume the owner chain first",
                resume.child_thread_id, resume.parent_thread_id
            )));
        }

        let session_source = if self
            .current_agent_alias(resume.child_thread_id)
            .await?
            .is_some()
        {
            owner
                .canonical_controlled_resume_source(
                    resume.child_thread_id,
                    resume.session_source.clone(),
                )
                .await?
        } else {
            resume.session_source.clone()
        };
        let restored_thread = match resume.edge_status {
            ThreadSpawnEdgeStatus::Open => {
                owner
                    .ensure_v2_agent_loaded_from_history(
                        config.clone(),
                        resume.child_thread_id,
                        session_source.clone(),
                        initial_history.clone(),
                        client_mcp_extensions.clone(),
                    )
                    .await?
            }
            ThreadSpawnEdgeStatus::Closed => {
                owner
                    .resume_v2_agent_from_history(
                        config.clone(),
                        resume.child_thread_id,
                        session_source,
                        initial_history.clone(),
                        client_mcp_extensions.clone(),
                    )
                    .await?
            }
        };
        Ok(Some(NewThread {
            thread_id: resume.child_thread_id,
            session_configured: restored_thread
                .startup_metadata()
                .to_session_configured_event(initial_history.get_event_msgs()),
            thread: restored_thread,
        }))
    }
}

impl ThreadManagerState {
    async fn persisted_v2_spawn_resume(
        &self,
        initial_history: &InitialHistory,
    ) -> CodexResult<Option<PersistedV2SpawnResume>> {
        resolve_persisted_v2_spawn_resume(initial_history, self.agent_graph_store()).await
    }
}

impl ThreadManagerState {
    /// Apply exact-runtime bookkeeping while the matching runtime is removed.
    pub(crate) async fn remove_thread_if_matches_with(
        &self,
        thread_id: &ThreadId,
        expected: &Arc<CodexThread>,
        on_remove: impl FnOnce() + Send,
    ) -> Option<Arc<CodexThread>> {
        let mut threads = self.threads.write().await;
        if threads
            .get(thread_id)
            .is_some_and(|thread| Arc::ptr_eq(thread, expected))
        {
            expected.session.prepare_for_thread_removal();
            let removed = threads.remove(thread_id);
            if removed.is_some() {
                on_remove();
            }
            removed
        } else {
            None
        }
    }

    pub(crate) async fn run_if_thread_absent(
        &self,
        thread_id: ThreadId,
        action: impl FnOnce() + Send,
    ) {
        let threads = self.threads.write().await;
        if !threads.contains_key(&thread_id) {
            action();
        }
    }

    pub(crate) async fn publish_restored_thread(
        &self,
        thread: &Arc<CodexThread>,
        parent: Option<&Arc<CodexThread>>,
        commit_metadata: impl FnOnce() -> CodexResult<()> + Send,
    ) -> CodexResult<()> {
        let mut threads = self.threads.write().await;
        if let Some(parent) = parent
            && (!parent.is_running()
                || !threads
                    .get(&parent.session.thread_id())
                    .is_some_and(|current| Arc::ptr_eq(current, parent)))
        {
            return Err(CodexErr::InvalidRequest(
                "restoration owner changed while loading the child".to_string(),
            ));
        }
        let thread_id = thread.session.thread_id();
        if threads
            .get(&thread_id)
            .is_some_and(|current| !Arc::ptr_eq(current, thread))
        {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {thread_id} was replaced while restoring"
            )));
        }
        thread.session.submission_admission.check_ready()?;
        let _thread_admission = thread
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .ok_or_else(|| CodexErr::InvalidRequest("restored runtime is closing".to_string()))?;
        let _parent_admission = if let Some(parent) = parent {
            parent.session.submission_admission.check_ready()?;
            Some(
                parent
                    .session
                    .submission_admission
                    .try_accept_completion_delivery()
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest("restoration owner is closing".to_string())
                    })?,
            )
        } else {
            None
        };
        commit_metadata()?;
        threads.insert(thread_id, Arc::clone(thread));
        drop(threads);
        let control = thread.session.services.agent_control.clone();
        tokio::spawn(async move {
            if let Err(error) = control.refresh_subtree_messaging(thread_id).await {
                tracing::warn!(%error, "failed to refresh published messaging context");
            }
        });
        Ok(())
    }
}

async fn resolve_persisted_v2_spawn_resume(
    initial_history: &InitialHistory,
    agent_graph_store: Option<Arc<dyn AgentGraphStore>>,
) -> CodexResult<Option<PersistedV2SpawnResume>> {
    let InitialHistory::Resumed(resumed) = initial_history else {
        return Ok(None);
    };
    if initial_history.get_multi_agent_version() != Some(MultiAgentVersion::V2) {
        return Ok(None);
    }
    let Some(session_meta) = resumed.history.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == resumed.conversation_id => {
            Some(&meta_line.meta)
        }
        RolloutItem::SessionMeta(_)
        | RolloutItem::RealtimeItem(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::AgentResponseObservation(_)
        | RolloutItem::Compacted(_)
        | RolloutItem::RetainedContext(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::EventMsg(_) => None,
    }) else {
        return Ok(None);
    };
    if !session_meta.source.is_non_root_agent()
        && agent_graph_store
            .as_ref()
            .is_none_or(|graph| !graph.supports_agent_aliases())
    {
        return Ok(None);
    }
    let agent_graph_store = agent_graph_store.ok_or_else(|| {
        CodexErr::InvalidRequest(format!(
            "cannot resume spawned V2 child {} because its persisted agent graph is unavailable; restore the graph state and retry",
            resumed.conversation_id
        ))
    })?;
    let alias = if agent_graph_store.supports_agent_aliases() {
        agent_graph_store
            .find_current_agent_alias_by_thread(resumed.conversation_id)
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?
    } else {
        None
    };
    let (session_id, parent_thread_id) = match alias {
        Some(alias) if ThreadId::from(alias.session_id) == resumed.conversation_id => {
            return Ok(None);
        }
        Some(alias) => {
            let parent = agent_graph_store
                .find_thread_spawn_parent(resumed.conversation_id)
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("owned V2 child has no parent edge".into())
                })?;
            (alias.session_id, parent)
        }
        None => match session_meta.source.parent_thread_id() {
            Some(parent) => (session_meta.session_id, parent),
            None => return Ok(None),
        },
    };

    let closed_children = agent_graph_store
        .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Closed))
        .await
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to inspect closed thread-spawn edge for {}: {err}",
                resumed.conversation_id
            ))
        })?;
    let edge_status = if closed_children.contains(&resumed.conversation_id) {
        ThreadSpawnEdgeStatus::Closed
    } else {
        let open_children = agent_graph_store
            .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Open))
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to inspect open thread-spawn edge for {}: {err}",
                    resumed.conversation_id
                ))
            })?;
        if !open_children.contains(&resumed.conversation_id) {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} because its persisted thread-spawn edge from parent {parent_thread_id} is missing; restore the graph state and retry",
                resumed.conversation_id
            )));
        }
        ThreadSpawnEdgeStatus::Open
    };

    Ok(Some(PersistedV2SpawnResume {
        child_thread_id: resumed.conversation_id,
        parent_thread_id,
        session_id,
        session_source: session_meta.source.clone(),
        edge_status,
    }))
}

#[cfg(test)]
#[path = "v2_spawn_resume_tests.rs"]
mod tests;
