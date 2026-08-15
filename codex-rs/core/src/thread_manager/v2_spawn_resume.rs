use super::*;
use crate::agent::control::LiveAgentMetadataDisposition;
use crate::agent::control::agent_alias_lifecycle_status;
use codex_agent_graph_store::AgentAliasState;
use codex_agent_graph_store::AgentGraphStoreError;
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
    agent_graph_store: Arc<dyn AgentGraphStore>,
}

async fn restore_failed_v2_spawn_lifecycle(
    agent_graph_store: &Arc<dyn AgentGraphStore>,
    session_id: SessionId,
    child_thread_id: ThreadId,
    edge_status: ThreadSpawnEdgeStatus,
) -> Result<(), String> {
    if !agent_graph_store.supports_agent_aliases() {
        return agent_graph_store
            .set_thread_spawn_edge_status(child_thread_id, edge_status)
            .await
            .map_err(|err| {
                format!(
                    "failed to restore the persisted thread-spawn edge to {edge_status:?}: {err}"
                )
            });
    }

    let alias = agent_graph_store
        .find_agent_alias_by_thread(session_id, child_thread_id)
        .await
        .map_err(|err| {
            format!("failed to inspect the persisted agent lifecycle before rollback: {err}")
        })?;
    match alias {
        Some(alias) if alias.state == AgentAliasState::Transferred => Ok(()),
        Some(_) => {
            let restored = agent_graph_store
                .set_agent_lifecycle_state(session_id, child_thread_id, edge_status)
                .await
                .map_err(|err| {
                    format!(
                        "failed to restore the persisted agent lifecycle to {edge_status:?}: {err}"
                    )
                })?;
            if restored {
                Ok(())
            } else {
                Err(format!(
                    "persisted agent lifecycle disappeared during rollback for {child_thread_id}"
                ))
            }
        }
        None => agent_graph_store
            .set_thread_spawn_edge_status(child_thread_id, edge_status)
            .await
            .map_err(|err| {
                format!(
                    "failed to restore the persisted thread-spawn edge to {edge_status:?}: {err}"
                )
            }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn cleanup_failed_v2_spawn_resume(
    state: &Arc<ThreadManagerState>,
    owner: &AgentControl,
    child_thread: &Arc<CodexThread>,
    runtime_origin: ThreadRuntimeOrigin,
    previous_metadata: Option<&crate::agent::AgentMetadata>,
    attempt_metadata: Option<&crate::agent::AgentMetadata>,
    lifecycle_restore: Option<(
        &Arc<dyn AgentGraphStore>,
        SessionId,
        ThreadId,
        ThreadSpawnEdgeStatus,
    )>,
    resume_error: CodexErr,
) -> CodexErr {
    let child_thread_id = child_thread.session.thread_id();
    let metadata_disposition = match lifecycle_restore {
        Some((_, _, _, ThreadSpawnEdgeStatus::Open)) => LiveAgentMetadataDisposition::Preserve,
        Some((_, _, _, ThreadSpawnEdgeStatus::Closed)) | None => {
            LiveAgentMetadataDisposition::Release
        }
    };
    let terminal_presentation_disarm = (runtime_origin == ThreadRuntimeOrigin::Created)
        .then(|| child_thread.session.disarm_terminal_presentation());
    let lifecycle_restore_result = match lifecycle_restore {
        Some((agent_graph_store, session_id, persisted_child_thread_id, edge_status)) => {
            let child_is_current_or_unpublished = state
                .thread_instance_is_current_or_pending(child_thread)
                .await;
            if child_is_current_or_unpublished {
                restore_failed_v2_spawn_lifecycle(
                    agent_graph_store,
                    session_id,
                    persisted_child_thread_id,
                    edge_status,
                )
                .await
            } else {
                Ok(())
            }
        }
        None => Ok(()),
    };
    let (shutdown_result, forced_cleanup_result) = match runtime_origin {
        ThreadRuntimeOrigin::Created => {
            let shutdown_result = owner
                .shutdown_live_agent_instance(child_thread, metadata_disposition)
                .await;
            let forced_cleanup_result = match &shutdown_result {
                Ok(_) => None,
                Err(_) => Some(
                    owner
                        .discard_live_agent_instance(child_thread, metadata_disposition)
                        .await,
                ),
            };
            (Some(shutdown_result), forced_cleanup_result)
        }
        // Another caller installed this runtime before the resume attempt reached the live-thread
        // lookup. This attempt owns its metadata/edge mutations, but never the runtime itself.
        ThreadRuntimeOrigin::Existing => (None, None),
    };
    let metadata_restore_result = match runtime_origin {
        ThreadRuntimeOrigin::Created => Ok(()),
        // Metadata remains independently mutable while an adopted runtime is live. Restore the
        // pre-attempt snapshot only while the registry still contains the exact value installed
        // by this attempt; an explicit close or newer task update is authoritative.
        ThreadRuntimeOrigin::Existing => match (attempt_metadata, previous_metadata) {
            (Some(attempt_metadata), Some(previous_metadata)) => owner
                .restore_agent_metadata_if_current(
                    child_thread_id,
                    attempt_metadata,
                    previous_metadata.clone(),
                )
                .map(drop)
                .map_err(|err| format!("failed to restore preexisting agent metadata: {err}")),
            (Some(attempt_metadata), None) => {
                let _ = owner.clear_agent_metadata_if_current(child_thread_id, attempt_metadata);
                Ok(())
            }
            (None, Some(_) | None) => Ok(()),
        },
    };
    if let Some(terminal_presentation_disarm) = terminal_presentation_disarm {
        terminal_presentation_disarm.commit();
    }

    let mut cleanup_errors = Vec::new();
    if let Err(lifecycle_restore_error) = lifecycle_restore_result {
        cleanup_errors.push(lifecycle_restore_error);
    }
    if let Some(Err(shutdown_error)) = shutdown_result {
        cleanup_errors.push(format!(
            "graceful runtime shutdown failed: {shutdown_error}"
        ));
    }
    if let Some(Err(forced_cleanup_error)) = forced_cleanup_result {
        cleanup_errors.push(format!(
            "forced runtime discard also failed: {forced_cleanup_error}"
        ));
    }
    if let Err(metadata_restore_error) = metadata_restore_result {
        cleanup_errors.push(metadata_restore_error);
    }
    if cleanup_errors.is_empty() {
        resume_error
    } else {
        CodexErr::Fatal(format!(
            "{resume_error}; rollback encountered additional failures: {}",
            cleanup_errors.join("; ")
        ))
    }
}

#[allow(clippy::too_many_arguments)]
async fn cleanup_failed_v2_spawn_resume_result(
    state: &Arc<ThreadManagerState>,
    owner: &AgentControl,
    restored: &mut ThreadSpawnResult,
    previous_metadata: Option<&crate::agent::AgentMetadata>,
    attempt_metadata: Option<&crate::agent::AgentMetadata>,
    lifecycle_restore: Option<(
        &Arc<dyn AgentGraphStore>,
        SessionId,
        ThreadId,
        ThreadSpawnEdgeStatus,
    )>,
    resume_error: CodexErr,
) -> CodexErr {
    let error = cleanup_failed_v2_spawn_resume(
        state,
        owner,
        &restored.thread,
        restored.runtime_origin,
        previous_metadata,
        attempt_metadata,
        lifecycle_restore,
        resume_error,
    )
    .await;
    restored.disarm_setup_cleanup();
    error
}

impl ThreadManager {
    /// Resume a persisted V2 spawned child through its live owning control plane.
    ///
    /// A graph-backed child must not fall through to generic cold resume once its persisted
    /// parent/child edge has been recognized. Doing so would construct a detached `AgentControl`
    /// and leave path routing, completion delivery, and residency accounting split across two
    /// control planes. When the direct parent or its matching control identity is unavailable,
    /// return a recoverable error so callers can resume the owner chain first.
    pub(super) async fn try_resume_persisted_v2_spawn(
        &self,
        config: &Config,
        initial_history: &InitialHistory,
        client_mcp_extensions: &ClientMcpExtensions,
    ) -> CodexResult<Option<NewThread>> {
        let (resume, _parent_resume_guard, _resume_guard) = loop {
            let Some(initial_resume) = self
                .state
                .persisted_v2_spawn_resume(initial_history)
                .await?
            else {
                return Ok(None);
            };
            let parent_resume_lock = self
                .state
                .agent_lifecycle_lock(initial_resume.parent_thread_id);
            let parent_resume_guard = parent_resume_lock.lock_owned().await;
            let resume_lock = self
                .state
                .agent_lifecycle_lock(initial_resume.child_thread_id);
            let resume_guard = resume_lock.lock_owned().await;
            let Some(resume) = self
                .state
                .persisted_v2_spawn_resume(initial_history)
                .await?
            else {
                return Ok(None);
            };
            if resume.parent_thread_id != initial_resume.parent_thread_id {
                // Adoption can commit while this cold resume waits for the target lock. The
                // re-resolved parent is authoritative, but setup must hold that parent's lifecycle
                // boundary before publishing beneath it. Drop both stale guards and retry in
                // parent-before-child order.
                continue;
            }
            break (resume, parent_resume_guard, resume_guard);
        };

        let parent_thread = self
            .state
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

        let previous_child_metadata = owner.get_agent_metadata(resume.child_thread_id);
        let mut restored = match resume.edge_status {
            ThreadSpawnEdgeStatus::Open => {
                // Open descendants are normally restored with their root metadata. Re-run the
                // idempotent metadata pass so an explicitly unloaded child can still be resumed.
                owner
                    .restore_v2_agent_metadata(config, ThreadId::from(resume.session_id))
                    .await;
                owner
                    .ensure_v2_agent_loaded_from_history(
                        config.clone(),
                        resume.child_thread_id,
                        resume.session_source.clone(),
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
                        resume.session_source.clone(),
                        initial_history.clone(),
                        client_mcp_extensions.clone(),
                    )
                    .await?
            }
        };
        let runtime_origin = restored.runtime_origin;
        let restored_thread = Arc::clone(&restored.thread);
        let restored_child_metadata = owner.get_agent_metadata(resume.child_thread_id);
        let restored_thread_id = restored_thread.session.thread_id();
        if restored_thread_id != resume.child_thread_id {
            return Err(cleanup_failed_v2_spawn_resume_result(
                &self.state,
                &owner,
                &mut restored,
                previous_child_metadata.as_ref(),
                restored_child_metadata.as_ref(),
                Some((
                    &resume.agent_graph_store,
                    resume.session_id,
                    resume.child_thread_id,
                    resume.edge_status,
                )),
                CodexErr::Fatal(format!(
                    "restored spawned V2 child {} as unexpected thread {restored_thread_id}",
                    resume.child_thread_id
                )),
            )
            .await);
        }

        let owner_is_still_current = self
            .state
            .get_thread(resume.parent_thread_id)
            .await
            .is_ok_and(|current| Arc::ptr_eq(&current, &parent_thread));
        if !owner_is_still_current {
            return Err(
                cleanup_failed_v2_spawn_resume_result(
                    &self.state,
                    &owner,
                    &mut restored,
                    previous_child_metadata.as_ref(),
                    restored_child_metadata.as_ref(),
                    Some((
                        &resume.agent_graph_store,
                        resume.session_id,
                        resume.child_thread_id,
                        resume.edge_status,
                    )),
                    CodexErr::InvalidRequest(format!(
                        "cannot resume spawned V2 child {} because its direct parent {} stopped while restoration was in progress; resume the parent and retry",
                        resume.child_thread_id, resume.parent_thread_id
                    )),
                )
                .await,
            );
        }

        let open_children = match resume
            .agent_graph_store
            .list_thread_spawn_children(resume.parent_thread_id, Some(ThreadSpawnEdgeStatus::Open))
            .await
        {
            Ok(open_children) => open_children,
            Err(err) => {
                return Err(cleanup_failed_v2_spawn_resume_result(
                    &self.state,
                    &owner,
                    &mut restored,
                    previous_child_metadata.as_ref(),
                    restored_child_metadata.as_ref(),
                    Some((
                        &resume.agent_graph_store,
                        resume.session_id,
                        resume.child_thread_id,
                        resume.edge_status,
                    )),
                    CodexErr::Fatal(format!(
                        "failed to verify reopened thread-spawn edge for {}: {err}",
                        resume.child_thread_id
                    )),
                )
                .await);
            }
        };
        if !open_children.contains(&resume.child_thread_id) {
            return Err(cleanup_failed_v2_spawn_resume_result(
                &self.state,
                &owner,
                &mut restored,
                previous_child_metadata.as_ref(),
                restored_child_metadata.as_ref(),
                Some((
                    &resume.agent_graph_store,
                    resume.session_id,
                    resume.child_thread_id,
                    resume.edge_status,
                )),
                CodexErr::Fatal(format!(
                    "restored spawned V2 child {} without reopening its persisted edge",
                    resume.child_thread_id
                )),
            )
            .await);
        }

        let runtime_publication = if runtime_origin == ThreadRuntimeOrigin::Created {
            self.state.publish_thread(&restored_thread).await
        } else if self
            .state
            .get_thread(resume.child_thread_id)
            .await
            .is_ok_and(|current| Arc::ptr_eq(&current, &restored_thread))
        {
            Ok(())
        } else {
            Err(CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} because its runtime was replaced while \
                 restoration was in progress; retry against the current runtime",
                resume.child_thread_id
            )))
        };
        if let Err(err) = runtime_publication {
            return Err(cleanup_failed_v2_spawn_resume_result(
                &self.state,
                &owner,
                &mut restored,
                previous_child_metadata.as_ref(),
                restored_child_metadata.as_ref(),
                Some((
                    &resume.agent_graph_store,
                    resume.session_id,
                    resume.child_thread_id,
                    resume.edge_status,
                )),
                err,
            )
            .await);
        }
        restored.disarm_setup_cleanup();
        if runtime_origin == ThreadRuntimeOrigin::Created {
            self.state.notify_thread_created(resume.child_thread_id);
        }
        Ok(Some(NewThread {
            thread_id: resume.child_thread_id,
            session_configured: restored_thread.session_configured(),
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
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::AgentResponseObservation(_)
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::RealtimeItem(_)
        | RolloutItem::EventMsg(_) => None,
    }) else {
        return Ok(None);
    };
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: historical_parent_thread_id,
        agent_path,
        agent_role,
        ..
    }) = &session_meta.source
    else {
        return Ok(None);
    };
    let agent_graph_store = agent_graph_store.ok_or_else(|| {
        CodexErr::InvalidRequest(format!(
            "cannot resume spawned V2 child {} because its persisted agent graph is unavailable; restore the graph state and retry",
            resumed.conversation_id
        ))
    })?;
    let current_alias = match agent_graph_store
        .find_current_agent_alias_by_thread(resumed.conversation_id)
        .await
    {
        Ok(Some(alias)) => alias,
        Ok(None) => {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} because it has no current durable owner or persisted thread-spawn edge; restore the graph state and retry",
                resumed.conversation_id
            )));
        }
        Err(AgentGraphStoreError::InvalidRequest { message }) => {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} because its persisted thread-spawn edge and agent graph are unavailable: {message}; restore the graph state and retry",
                resumed.conversation_id
            )));
        }
        Err(err) => {
            return Err(CodexErr::Fatal(format!(
                "failed to resolve the current durable owner for spawned V2 child {}: {err}",
                resumed.conversation_id
            )));
        }
    };
    let edge_status = agent_alias_lifecycle_status(current_alias.state).ok_or_else(|| {
        CodexErr::InvalidRequest(format!(
            "cannot resume spawned V2 child {} through a transferred historical alias; resolve its current owner and retry",
            resumed.conversation_id
        ))
    })?;
    let parent_thread_id = agent_graph_store
        .find_thread_spawn_parent(resumed.conversation_id)
        .await
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to resolve the current parent for spawned V2 child {}: {err}",
                resumed.conversation_id
            ))
        })?
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "cannot resume spawned V2 child {} because its current parent edge is missing; restore the graph state and retry",
                resumed.conversation_id
            ))
        })?;
    let root_thread_id = ThreadId::from(current_alias.session_id);
    let mut ancestor_thread_id = parent_thread_id;
    let mut depth = 1usize;
    let mut visited = HashSet::from([resumed.conversation_id]);
    while ancestor_thread_id != root_thread_id {
        if !visited.insert(ancestor_thread_id) {
            return Err(CodexErr::InvalidRequest(format!(
                "spawned V2 child {} belongs to a cyclic persisted spawn graph",
                resumed.conversation_id
            )));
        }
        ancestor_thread_id = agent_graph_store
            .find_thread_spawn_parent(ancestor_thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to resolve current ancestry for spawned V2 child {}: {err}",
                    resumed.conversation_id
                ))
            })?
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "cannot resume spawned V2 child {} because its current ancestry does not reach owner {}",
                    resumed.conversation_id, current_alias.session_id
                ))
            })?;
        depth = depth.checked_add(1).ok_or_else(|| {
            CodexErr::Fatal(format!(
                "persisted ancestry for spawned V2 child {} exceeds supported depth",
                resumed.conversation_id
            ))
        })?;
    }
    let depth = i32::try_from(depth).map_err(|_| {
        CodexErr::Fatal(format!(
            "persisted ancestry for spawned V2 child {} exceeds supported depth",
            resumed.conversation_id
        ))
    })?;
    let preserves_historical_path = current_alias.session_id == session_meta.session_id
        && parent_thread_id == *historical_parent_thread_id;
    let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth,
        agent_path: preserves_historical_path
            .then(|| agent_path.clone())
            .flatten(),
        agent_nickname: current_alias.nickname,
        agent_role: agent_role.clone(),
    });

    Ok(Some(PersistedV2SpawnResume {
        child_thread_id: resumed.conversation_id,
        parent_thread_id,
        session_id: current_alias.session_id,
        session_source,
        edge_status,
        agent_graph_store,
    }))
}

#[cfg(test)]
#[path = "v2_spawn_resume_tests.rs"]
mod tests;
