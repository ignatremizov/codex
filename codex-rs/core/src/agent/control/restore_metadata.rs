//! Exact persisted identity and configuration checks shared by restoration paths.

use super::resume_role::apply_resumed_agent_role;
use super::spawn::load_agent_model_context;
use super::*;
use crate::codex_thread::CodexThread;

pub(super) async fn apply_restored_v2_agent_role(
    config: &mut Config,
    session_source: &SessionSource,
) -> CodexResult<()> {
    if let Some(role) = session_source.get_agent_role() {
        apply_resumed_agent_role(config, &role).await?;
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
        let loaded_control = &thread.session.services.agent_control;
        // A standalone root keeps its persisted session identity when another root adopts it.
        // Spawned descendants must remain attached to the exact owning control session.
        let matches_control_session = !thread.session_source.is_non_root_agent()
            || self.session_id() == loaded_control.session_id();
        let matches_owner =
            Arc::ptr_eq(&self.state, &loaded_control.state) && matches_control_session;
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
        self.state.register_root_thread(root_thread_id);

        let Ok(state) = self.upgrade() else {
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

        for thread_id in descendant_ids {
            if self.state.agent_metadata_for_thread(thread_id).is_some() {
                continue;
            }
            let restore_result = async {
                let stored_thread = state
                    .read_stored_thread(ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await?;
                let stored_agent_path = stored_thread
                    .agent_path
                    .as_deref()
                    .map(AgentPath::try_from)
                    .transpose()
                    .map_err(|err| {
                        CodexErr::InvalidRequest(format!("invalid stored agent path: {err}"))
                    })?;
                let canonical_source =
                    load_agent_model_context(&state, thread_id, stored_thread.history_mode)
                        .await?
                        .and_then(|history| {
                            InitialHistory::Resumed(ResumedHistory {
                                conversation_id: thread_id,
                                history: Arc::new(history),
                                rollout_path: stored_thread.rollout_path.clone(),
                            })
                            .get_resumed_session_sources()
                            .map(|(session_source, _)| session_source)
                        });
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
                let mut reservation = self.state.reserve_spawn_slot(/*max_threads*/ None)?;
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
                Ok::<(), CodexErr>(())
            }
            .await;
            if let Err(err) = restore_result {
                warn!("failed to restore V2 agent metadata for {thread_id}: {err}");
            }
        }
    }
}
