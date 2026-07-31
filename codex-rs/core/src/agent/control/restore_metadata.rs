//! Exact persisted identity and configuration checks shared by restoration paths.

use super::resume_role::apply_resumed_agent_role;
use super::spawn::load_agent_model_context;
use super::*;
use crate::codex_thread::CodexThread;
use futures::StreamExt;
use futures::stream;

/// Captured parent authority and its nonblocking lifecycle reservation.
/// The child must retain this exact runtime through publication, not resolve its UUID again.
pub(super) struct RestorationOwner {
    pub(super) parent: Option<Arc<CodexThread>>,
    _lifecycle: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl LocalAgentControl {
    pub(super) async fn capture_restoration_owner(
        &self,
        manager: &Arc<ThreadManagerState>,
        parent_id: Option<ThreadId>,
        expected_parent: Option<Arc<CodexThread>>,
        version: MultiAgentVersion,
    ) -> CodexResult<RestorationOwner> {
        let parent = match (parent_id, expected_parent) {
            (None, None) => None,
            (Some(id), Some(parent)) if parent.session.thread_id() == id => Some(parent),
            (Some(id), None) => Some(manager.get_thread(id).await.map_err(|_| {
                CodexErr::InvalidRequest(format!(
                    "restoration parent {id} is not loaded; resume the parent first"
                ))
            })?),
            _ => {
                return Err(CodexErr::InvalidRequest(
                    "recorded parent ownership is inconsistent".to_string(),
                ));
            }
        };
        let lifecycle = match &parent {
            Some(parent) => Some(manager.v2_spawn_resume_lock(parent.session.thread_id())
                .try_lock_owned().map_err(|_| CodexErr::InvalidRequest(
                    "restoration owner is busy; retry after its current lifecycle operation".to_string(),
                ))?),
            None => None,
        };
        if let Some(parent) = &parent {
            let current = manager.get_thread(parent.session.thread_id()).await?;
            if !Arc::ptr_eq(parent, &current)
                || !parent.is_running()
                || !self
                    .runtime
                    .shares_tree_with(&parent.session.services.local_agent_runtime)
                || (version == MultiAgentVersion::V2
                    && (parent.multi_agent_version() != Some(MultiAgentVersion::V2)
                        || parent.session.session_id() != self.session_id()))
            {
                return Err(CodexErr::InvalidRequest(
                    "restoration owner changed or belongs to another control".to_string(),
                ));
            }
            parent.session.submission_admission.check_ready()?;
        }
        Ok(RestorationOwner {
            parent,
            _lifecycle: lifecycle,
        })
    }
}

pub(super) async fn apply_restored_v2_agent_role(
    config: &mut Config,
    session_source: &SessionSource,
    developer_instructions_override: Option<String>,
) -> CodexResult<()> {
    if let Some(role) = session_source.get_agent_role() {
        apply_resumed_agent_role(config, &role).await?;
    }
    if let Some(instructions) = developer_instructions_override {
        config.developer_instructions = Some(instructions);
    }
    Ok(())
}

pub(super) fn apply_restored_agent_model(
    config: &mut Config,
    stored_model: Option<String>,
    stored_model_provider: String,
) -> CodexResult<()> {
    if let Some(model) = stored_model {
        config.model = Some(model);
    }
    if config.model_provider_id != stored_model_provider {
        config.model_provider = config
            .model_providers
            .get(&stored_model_provider)
            .cloned()
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "Model provider `{stored_model_provider}` not found"
                ))
            })?;
        config.model_provider_id = stored_model_provider;
    }
    Ok(())
}

pub(super) fn canonical_agent_path(session_source: &SessionSource) -> Option<AgentPath> {
    session_source
        .get_agent_path()
        .or_else(|| (!session_source.is_non_root_agent()).then(AgentPath::root))
}

impl LocalAgentControl {
    pub(super) fn validate_loaded_v2_agent(
        &self,
        thread: &Arc<CodexThread>,
        expected_session_source: Option<&SessionSource>,
    ) -> CodexResult<()> {
        thread.session.submission_admission.check_ready()?;
        let loaded_control = thread
            .session
            .services
            .local_agent_runtime
            .control(thread.session.session_id());
        // A standalone root keeps its persisted session identity when another root adopts it.
        // Spawned descendants must remain attached to the exact owning control session.
        let matches_control_session = !thread.session_source.is_non_root_agent()
            || self.session_id() == loaded_control.session_id();
        let matches_owner =
            self.runtime.shares_tree_with(&loaded_control.runtime) && matches_control_session;
        let matches_source = expected_session_source.is_none_or(|expected_session_source| {
            &thread.session_source == expected_session_source
        });
        if !thread.is_running()
            || thread.multi_agent_version() != Some(MultiAgentVersion::V2)
            || !matches_owner
            || !matches_source
        {
            return Err(CodexErr::InvalidRequest(format!(
                "loaded thread {} does not match its persisted V2 owner and session source",
                thread.session.thread_id()
            )));
        }
        Ok(())
    }

    pub(super) fn validate_loaded_rollout_path(
        &self,
        thread: &Arc<CodexThread>,
        expected_rollout_path: Option<&std::path::Path>,
    ) -> CodexResult<()> {
        if let Some(expected_rollout_path) = expected_rollout_path
            && thread.rollout_path().as_deref() != Some(expected_rollout_path)
        {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {} is already running with a different rollout path",
                thread.session.thread_id()
            )));
        }
        Ok(())
    }

    /// Restore persisted V2 agent identities without reopening their runtimes.
    pub(crate) async fn restore_v2_agent_metadata(
        &self,
        config: &Config,
        root_thread_id: ThreadId,
    ) {
        self.runtime.registry.register_root_thread(root_thread_id);

        let Ok(state) = self.runtime.upgrade() else {
            return;
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return;
        };
        let descendant_ids = match agent_graph_store
            .list_thread_spawn_descendants(
                root_thread_id,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
            )
            .await
        {
            Ok(descendant_ids) => descendant_ids,
            Err(err) => {
                warn!("failed to restore persisted V2 agent metadata for {root_thread_id}: {err}");
                return;
            }
        };

        let registry = &self.runtime.registry;
        // Keep upstream's bounded overlap and graph-order allocation. Reading canonical
        // metadata is new, but it must not serialize every descendant's storage access.
        let mut stored_threads = stream::iter(
            descendant_ids
                .into_iter()
                .filter(|thread_id| registry.agent_metadata_for_thread(*thread_id).is_none())
                .map(|thread_id| {
                    let state = &state;
                    async move {
                        let stored = async {
                            let stored_thread = state
                                .read_stored_thread(ReadThreadParams {
                                    thread_id,
                                    include_archived: true,
                                    include_history: false,
                                })
                                .await?;
                            let canonical_source = load_agent_model_context(
                                state,
                                thread_id,
                                stored_thread.history_mode,
                            )
                            .await?
                            .and_then(|history| {
                                InitialHistory::Resumed(ResumedHistory {
                                    conversation_id: thread_id,
                                    history: Arc::new(history),
                                    rollout_path: stored_thread.rollout_path.clone(),
                                })
                                .get_resumed_session_sources()
                                .map(|(source, _)| source)
                            });
                            Ok::<_, CodexErr>((stored_thread, canonical_source))
                        }
                        .await;
                        (thread_id, stored)
                    }
                }),
        )
        .buffered(/*n*/ 8);
        while let Some((thread_id, stored)) = stored_threads.next().await {
            if registry.agent_metadata_for_thread(thread_id).is_some() {
                continue;
            }
            let restore_result = stored.and_then(|(stored_thread, canonical_source)| {
                let stored_agent_path = stored_thread
                    .agent_path
                    .as_deref()
                    .map(AgentPath::try_from)
                    .transpose()
                    .map_err(|err| {
                        CodexErr::InvalidRequest(format!("invalid stored agent path: {err}"))
                    })?;
                let effective_source = canonical_source
                    .clone()
                    .unwrap_or_else(|| stored_thread.source.clone());
                if !matches!(
                    &effective_source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                ) {
                    return Err(CodexErr::InvalidRequest(format!(
                        "persisted V2 descendant {thread_id} has no canonical thread-spawn source"
                    )));
                }
                let mut reservation = self
                    .runtime
                    .registry
                    .reserve_spawn_slot(/*max_threads*/ None)?;
                let mut metadata = match canonical_source {
                    Some(canonical_source) => self.prepare_restored_agent_metadata_exact(
                        &mut reservation,
                        canonical_source.get_agent_path(),
                        canonical_source.get_agent_role(),
                        canonical_source.get_nickname(),
                    )?,
                    None => self.prepare_agent_metadata(
                        &mut reservation,
                        config,
                        stored_agent_path.or_else(|| stored_thread.source.get_agent_path()),
                        stored_thread
                            .agent_role
                            .or_else(|| stored_thread.source.get_agent_role()),
                        stored_thread
                            .agent_nickname
                            .or_else(|| stored_thread.source.get_nickname()),
                    )?,
                };
                metadata.agent_id = Some(thread_id);
                reservation.commit(metadata);
                Ok(())
            });
            if let Err(err) = restore_result {
                warn!("failed to restore V2 agent metadata for {thread_id}: {err}");
            }
        }
    }
}
