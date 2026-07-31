use super::*;
use crate::agent::control::LiveAgentMetadataDisposition;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;

struct PersistedV2SpawnResume {
    child_thread_id: ThreadId,
    parent_thread_id: ThreadId,
    session_id: SessionId,
    session_source: SessionSource,
    edge_status: ThreadSpawnEdgeStatus,
    agent_graph_store: Arc<dyn AgentGraphStore>,
}

async fn cleanup_failed_v2_spawn_resume(
    state: &Arc<ThreadManagerState>,
    owner: &AgentControl,
    child_thread: &Arc<CodexThread>,
    edge_restore: Option<(&Arc<dyn AgentGraphStore>, ThreadSpawnEdgeStatus)>,
    resume_error: CodexErr,
) -> CodexErr {
    let child_thread_id = child_thread.session.thread_id();
    let metadata_disposition = match edge_restore {
        Some((_, ThreadSpawnEdgeStatus::Open)) => LiveAgentMetadataDisposition::Preserve,
        Some((_, ThreadSpawnEdgeStatus::Closed)) | None => LiveAgentMetadataDisposition::Release,
    };
    let terminal_presentation_disarm = child_thread.session.disarm_terminal_presentation();
    let edge_restore_result = match edge_restore {
        Some((agent_graph_store, edge_status)) => {
            let threads = state.threads.read().await;
            let child_is_current = threads
                .get(&child_thread_id)
                .is_some_and(|current| Arc::ptr_eq(current, child_thread));
            if child_is_current {
                agent_graph_store
                    .set_thread_spawn_edge_status(child_thread_id, edge_status)
                    .await
                    .map_err(|err| {
                        format!(
                            "failed to restore the persisted thread-spawn edge to {edge_status:?}: {err}"
                        )
                    })
            } else {
                Ok(())
            }
        }
        None => Ok(()),
    };
    let shutdown_result = owner
        .shutdown_live_agent_instance(child_thread, metadata_disposition)
        .await;
    let forced_cleanup_result = match &shutdown_result {
        Ok(_) => Ok(()),
        Err(_) => {
            owner
                .discard_live_agent_instance(child_thread, metadata_disposition)
                .await
        }
    };
    terminal_presentation_disarm.commit();

    let mut cleanup_errors = Vec::new();
    if let Err(edge_restore_error) = edge_restore_result {
        cleanup_errors.push(edge_restore_error);
    }
    if let Err(shutdown_error) = shutdown_result {
        cleanup_errors.push(format!(
            "graceful runtime shutdown failed: {shutdown_error}"
        ));
    }
    if let Err(forced_cleanup_error) = forced_cleanup_result {
        cleanup_errors.push(format!(
            "forced runtime discard also failed: {forced_cleanup_error}"
        ));
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
        supports_openai_form_elicitation: bool,
    ) -> CodexResult<Option<NewThread>> {
        let Some(initial_resume) = self
            .state
            .persisted_v2_spawn_resume(initial_history)
            .await?
        else {
            return Ok(None);
        };
        let resume_lock = self
            .state
            .v2_spawn_resume_lock(initial_resume.child_thread_id);
        let _resume_guard = resume_lock.lock().await;
        let Some(resume) = self
            .state
            .persisted_v2_spawn_resume(initial_history)
            .await?
        else {
            return Ok(None);
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

        let restored_thread = match resume.edge_status {
            ThreadSpawnEdgeStatus::Open => {
                // Open descendants are normally restored with their root metadata. Re-run the
                // idempotent metadata pass so an explicitly unloaded child can still be resumed.
                owner
                    .restore_v2_agent_metadata(config, resume.parent_thread_id)
                    .await;
                owner
                    .ensure_v2_agent_loaded_from_history(
                        config.clone(),
                        resume.child_thread_id,
                        resume.session_source.clone(),
                        initial_history.clone(),
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
                    )
                    .await?
            }
        };
        let restored_thread_id = restored_thread.session.thread_id();
        if restored_thread_id != resume.child_thread_id {
            return Err(cleanup_failed_v2_spawn_resume(
                &self.state,
                &owner,
                &restored_thread,
                /*edge_restore*/ None,
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
                cleanup_failed_v2_spawn_resume(
                    &self.state,
                    &owner,
                    &restored_thread,
                    Some((&resume.agent_graph_store, resume.edge_status)),
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
                return Err(cleanup_failed_v2_spawn_resume(
                    &self.state,
                    &owner,
                    &restored_thread,
                    Some((&resume.agent_graph_store, resume.edge_status)),
                    CodexErr::Fatal(format!(
                        "failed to verify reopened thread-spawn edge for {}: {err}",
                        resume.child_thread_id
                    )),
                )
                .await);
            }
        };
        if !open_children.contains(&resume.child_thread_id) {
            return Err(cleanup_failed_v2_spawn_resume(
                &self.state,
                &owner,
                &restored_thread,
                Some((&resume.agent_graph_store, resume.edge_status)),
                CodexErr::Fatal(format!(
                    "restored spawned V2 child {} without reopening its persisted edge",
                    resume.child_thread_id
                )),
            )
            .await);
        }

        let child_is_still_current = self
            .state
            .get_thread(resume.child_thread_id)
            .await
            .is_ok_and(|current| Arc::ptr_eq(&current, &restored_thread));
        if !child_is_still_current {
            return Err(
                cleanup_failed_v2_spawn_resume(
                    &self.state,
                    &owner,
                    &restored_thread,
                    Some((&resume.agent_graph_store, resume.edge_status)),
                    CodexErr::InvalidRequest(format!(
                        "cannot resume spawned V2 child {} because its runtime was replaced while restoration was in progress; retry against the current runtime",
                        resume.child_thread_id
                    )),
                )
                .await,
            );
        }
        if let Err(err) = restored_thread
            .set_openai_form_elicitation_support(supports_openai_form_elicitation)
            .await
        {
            return Err(
                cleanup_failed_v2_spawn_resume(
                    &self.state,
                    &owner,
                    &restored_thread,
                    Some((&resume.agent_graph_store, resume.edge_status)),
                    CodexErr::Fatal(format!(
                        "failed to update OpenAI form elicitation support for restored spawned V2 child {}: {err}",
                        resume.child_thread_id
                    )),
                )
                .await,
            );
        }
        Ok(Some(NewThread {
            thread_id: resume.child_thread_id,
            session_configured: restored_thread.session_configured(),
            thread: restored_thread,
        }))
    }
}

impl ThreadManagerState {
    pub(crate) fn v2_spawn_resume_lock(&self, thread_id: ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .v2_spawn_resume_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&thread_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(thread_id, Arc::downgrade(&lock));
        lock
    }

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
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::EventMsg(_) => None,
    }) else {
        return Ok(None);
    };
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id, ..
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

    let closed_children = agent_graph_store
        .list_thread_spawn_children(*parent_thread_id, Some(ThreadSpawnEdgeStatus::Closed))
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
            .list_thread_spawn_children(*parent_thread_id, Some(ThreadSpawnEdgeStatus::Open))
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
        parent_thread_id: *parent_thread_id,
        session_id: session_meta.session_id,
        session_source: session_meta.source.clone(),
        edge_status,
        agent_graph_store,
    }))
}

#[cfg(test)]
#[path = "v2_spawn_resume_tests.rs"]
mod tests;
