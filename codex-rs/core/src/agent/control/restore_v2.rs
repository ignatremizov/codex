//! Reload a V2 runtime using its canonical identity and the current owner's authority.

use super::restore_environments::bound_cached_environment_selections;
use super::restore_environments::explicit_workspace_environments;
use super::restore_metadata::apply_restored_agent_model;
use super::restore_metadata::apply_restored_v2_agent_role;
use super::restore_metadata::canonical_agent_path;
use super::spawn::load_agent_model_context;
use super::*;
use crate::agent::child_config::build_agent_resume_config;
use crate::agents_md_manager::SessionInstructions;
use crate::codex_thread::CodexThread;
use codex_protocol::mcp::ClientMcpExtensions;

impl LocalAgentControl {
    pub(crate) async fn ensure_v2_agent_loaded(
        &self,
        config: Config,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        let control = self.clone();
        tokio::spawn(async move {
            control
                .ensure_v2_agent_loaded_serialized(config, thread_id)
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent restoration worker failed: {error}")))?
    }

    async fn ensure_v2_agent_loaded_serialized(
        &self,
        config: Config,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let resume_lock = state.v2_spawn_resume_lock(thread_id);
        let _resume_guard = resume_lock.lock_owned().await;
        state.check_restoration_fence(thread_id)?;
        if let Ok(thread) = state.get_thread(thread_id).await {
            return self
                .ensure_v2_agent_loaded_from_source_and_history(
                    config,
                    thread_id,
                    thread.session_source.clone(),
                    /*initial_history_override*/ None,
                    /*client_mcp_extensions_override*/ None,
                )
                .await
                .map(drop);
        }
        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let history = load_agent_model_context(&state, thread_id, stored_thread.history_mode)
            .await?
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        if initial_history.get_multi_agent_version() != Some(MultiAgentVersion::V2) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let canonical_session_source = initial_history
            .get_resumed_session_sources()
            .map(|(session_source, _)| session_source)
            .unwrap_or(stored_thread.source);
        let canonical_session_source = if self.current_agent_alias(thread_id).await?.is_some() {
            self.require_current_agent_ownership(thread_id).await?;
            self.canonical_controlled_resume_source(thread_id, canonical_session_source)
                .await?
        } else {
            canonical_session_source
        };
        self.ensure_v2_agent_loaded_from_source_and_history(
            config,
            thread_id,
            canonical_session_source,
            Some(initial_history),
            /*client_mcp_extensions_override*/ None,
        )
        .await
        .map(drop)
    }

    pub(crate) async fn ensure_v2_agent_loaded_from_history(
        &self,
        config: Config,
        thread_id: ThreadId,
        canonical_session_source: SessionSource,
        initial_history: InitialHistory,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<Arc<CodexThread>> {
        self.ensure_v2_agent_loaded_from_source_and_history(
            config,
            thread_id,
            canonical_session_source,
            Some(initial_history),
            Some(client_mcp_extensions),
        )
        .await
    }

    async fn ensure_v2_agent_loaded_from_source_and_history(
        &self,
        config: Config,
        thread_id: ThreadId,
        canonical_session_source: SessionSource,
        initial_history_override: Option<InitialHistory>,
        client_mcp_extensions_override: Option<ClientMcpExtensions>,
    ) -> CodexResult<Arc<CodexThread>> {
        let state = self.upgrade()?;
        state.check_restoration_fence(thread_id)?;
        let parent = match canonical_session_source.parent_thread_id() {
            Some(parent_id) => Some(state.get_thread(parent_id).await?),
            None => None,
        };
        let _parent_guard = match &parent {
            Some(parent) => Some(state.v2_spawn_resume_lock(parent.session.thread_id())
                .try_lock_owned().map_err(|_| CodexErr::InvalidRequest(
                    "restoration owner is busy; retry after its current lifecycle operation".to_string(),
                ))?),
            None => None,
        };
        if let Ok(thread) = state.get_thread(thread_id).await {
            self.validate_loaded_v2_agent(&thread, Some(&canonical_session_source))?;
            let expected_rollout_path = initial_history_override.as_ref().and_then(
                |initial_history| match initial_history {
                    InitialHistory::Resumed(resumed) => resumed.rollout_path.as_deref(),
                    InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                        None
                    }
                },
            );
            self.validate_loaded_rollout_path(&thread, expected_rollout_path)?;
            let last_task_message = self
                .state
                .agent_metadata_for_thread(thread_id)
                .and_then(|metadata| metadata.last_task_message);
            let metadata_replacement = self.state.reserve_agent_metadata_replacement(
                thread_id,
                AgentMetadata {
                    agent_id: Some(thread_id),
                    agent_path: canonical_agent_path(&canonical_session_source),
                    agent_nickname: canonical_session_source.get_nickname(),
                    agent_role: canonical_session_source.get_agent_role(),
                    last_task_message,
                },
            )?;
            state
                .publish_restored_thread(&thread, parent.as_ref(), || metadata_replacement.commit())
                .await?;
            self.touch_loaded_v2_residency(&state, thread_id).await;
            return Ok(thread);
        }
        let previous_metadata = self.state.agent_metadata_for_thread(thread_id);
        let last_task_message = previous_metadata
            .as_ref()
            .and_then(|metadata| metadata.last_task_message.clone());
        let metadata_replacement = self.state.reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: canonical_agent_path(&canonical_session_source),
                agent_nickname: canonical_session_source.get_nickname(),
                agent_role: canonical_session_source.get_agent_role(),
                last_task_message,
            },
        )?;
        let load_result = self
            .ensure_v2_agent_loaded_inner(
                config,
                thread_id,
                Some(canonical_session_source.clone()),
                initial_history_override,
                client_mcp_extensions_override,
            )
            .await;
        match load_result {
            Ok(thread) => {
                if let Err(error) = self
                    .publish_restored_agent(&thread, parent.as_ref(), || {
                        metadata_replacement.commit()
                    })
                    .await
                {
                    return Err(self.cleanup_unpublished_restoration(&thread, error).await);
                }
                self.state.clear_evicted_environments(thread_id);
                Ok(thread)
            }
            Err(err) => Err(err),
        }
    }

    async fn ensure_v2_agent_loaded_inner(
        &self,
        mut config: Config,
        thread_id: ThreadId,
        canonical_session_source: Option<SessionSource>,
        initial_history_override: Option<InitialHistory>,
        client_mcp_extensions_override: Option<ClientMcpExtensions>,
    ) -> CodexResult<Arc<CodexThread>> {
        let state = self.upgrade()?;
        let explicit_resume = client_mcp_extensions_override.is_some();
        let expected_rollout_path = initial_history_override
            .as_ref()
            .and_then(|initial_history| match initial_history {
                InitialHistory::Resumed(resumed) => resumed.rollout_path.clone(),
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
            });
        if let Ok(thread) = state.get_thread(thread_id).await {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {} became live while preparing restoration",
                thread.session.thread_id()
            )));
        }
        let mut environment_selections = self.state.evicted_environments(thread_id);
        let registered_metadata = self
            .state
            .agent_metadata_for_thread(thread_id)
            .or_else(|| {
                canonical_session_source
                    .as_ref()
                    .map(|canonical_session_source| AgentMetadata {
                        agent_id: Some(thread_id),
                        agent_path: canonical_session_source.get_agent_path(),
                        agent_nickname: canonical_session_source.get_nickname(),
                        agent_role: canonical_session_source.get_agent_role(),
                        last_task_message: None,
                    })
            })
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        let registered_parent_thread_id = registered_metadata
            .agent_path
            .as_ref()
            .and_then(|agent_path| agent_path.as_str().rsplit_once('/'))
            .and_then(|(parent_path, _)| AgentPath::try_from(parent_path).ok())
            .and_then(|parent_path| self.state.agent_id_for_path(&parent_path));

        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let stored_model = stored_thread.model.clone();
        let stored_model_provider = stored_thread.model_provider.clone();
        let stored_reasoning_effort = stored_thread.reasoning_effort.clone();
        let stored_service_tier = stored_thread.service_tier.clone();
        let (stored_source, stored_parent_thread_id, initial_history) =
            match initial_history_override {
                Some(initial_history) => {
                    let stored_source = canonical_session_source
                        .clone()
                        .or_else(|| {
                            initial_history
                                .get_resumed_session_sources()
                                .map(|(session_source, _)| session_source)
                        })
                        .unwrap_or_default();
                    let stored_parent_thread_id = initial_history
                        .get_resumed_parent_thread_id()
                        .or_else(|| stored_source.parent_thread_id());
                    (stored_source, stored_parent_thread_id, initial_history)
                }
                None => {
                    let stored_source = stored_thread.source.clone();
                    let stored_parent_thread_id = stored_thread.parent_thread_id;
                    let history =
                        load_agent_model_context(&state, thread_id, stored_thread.history_mode)
                            .await?
                            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
                    let initial_history = InitialHistory::Resumed(ResumedHistory {
                        conversation_id: thread_id,
                        history: Arc::new(history),
                        rollout_path: stored_thread.rollout_path,
                    });
                    (stored_source, stored_parent_thread_id, initial_history)
                }
            };
        if initial_history.get_multi_agent_version() != Some(MultiAgentVersion::V2) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let resumed_session_source = initial_history
            .get_resumed_session_sources()
            .map(|(session_source, _)| session_source)
            .unwrap_or_else(|| stored_source.clone());
        let has_canonical_session_source = canonical_session_source.is_some();
        let session_source = canonical_session_source.unwrap_or_else(|| {
            if resumed_session_source.is_non_root_agent() {
                resumed_session_source
            } else if let (Some(parent_thread_id), Some(agent_path)) = (
                registered_parent_thread_id,
                registered_metadata.agent_path.clone(),
            ) {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: i32::try_from(
                        agent_path.as_str().matches('/').count().saturating_sub(1),
                    )
                    .unwrap_or(i32::MAX),
                    agent_path: Some(agent_path),
                    agent_nickname: registered_metadata.agent_nickname.clone(),
                    agent_role: registered_metadata.agent_role.clone(),
                })
            } else if stored_source.is_non_root_agent() {
                stored_source
            } else {
                resumed_session_source
            }
        });
        let parent_thread_id = if has_canonical_session_source {
            session_source.parent_thread_id()
        } else {
            initial_history
                .get_resumed_parent_thread_id()
                .or(stored_parent_thread_id)
                .or(registered_parent_thread_id)
                .or_else(|| session_source.parent_thread_id())
        };
        let parent_context = if let Some(parent_thread_id) = parent_thread_id {
            match state.get_thread(parent_thread_id).await {
                Ok(parent)
                    if parent.is_running()
                        && parent.multi_agent_version() == Some(MultiAgentVersion::V2)
                        && Arc::ptr_eq(
                            &self.state,
                            &parent.session.services.agent_control.state,
                        ) =>
                {
                    let turn = parent
                        .session
                        .new_turn_with_default_settings(
                            Uuid::now_v7().to_string(),
                            Default::default(),
                        )
                        .await;
                    if !explicit_resume {
                        config = build_agent_resume_config(&turn).map_err(|_| {
                        CodexErr::InvalidRequest(format!(
                            "cannot resume multi-agent v2 child {thread_id} with the current parent settings"
                        ))
                        })?;
                    }
                    Some((parent, turn.initial_environments.clone()))
                }
                Ok(_) => {
                    return Err(CodexErr::InvalidRequest(format!(
                        "cannot resume multi-agent v2 child {thread_id}: parent ownership is unavailable"
                    )));
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        apply_restored_v2_agent_role(&mut config, &session_source).await?;
        if config.multi_agent_version_override() == Some(MultiAgentVersion::Disabled) {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot restore persisted V2 child {thread_id} while agents are disabled"
            )));
        }
        config.service_tier = stored_service_tier;
        apply_restored_agent_model(&mut config, stored_model, stored_model_provider)?;
        config.model_reasoning_effort = stored_reasoning_effort;
        let (inherited_environments, inherited_exec_policy, client_mcp_extensions_override) =
            if let Some((parent, parent_environments)) = parent_context.as_ref() {
                let parent_config = parent.session.get_config().await;
                if !crate::exec_policy::child_uses_parent_exec_policy(&parent_config, &config) {
                    return Err(CodexErr::InvalidRequest(format!(
                        "cannot resume multi-agent v2 child {thread_id}: parent execution policy has changed; retry through the parent"
                    )));
                }
                let inherited_environments = if config.workspace_roots_explicit {
                    let explicit = explicit_workspace_environments(&config, parent_environments)?;
                    // Discard only this attempt's stale cached selection after the explicit
                    // override is prepared. Registry state is cleared after publication succeeds.
                    environment_selections = None;
                    explicit
                } else {
                    parent_environments.clone()
                };
                if let Some(selections) = environment_selections.as_mut() {
                    bound_cached_environment_selections(
                        selections,
                        parent_environments,
                        thread_id,
                    )?;
                }
                (
                    Some(inherited_environments),
                    Some(Arc::clone(&parent.session.services.exec_policy)),
                    client_mcp_extensions_override.or_else(|| Some(parent.client_mcp_extensions())),
                )
            } else {
                (
                    self.inherited_environments_for_source(&state, Some(&session_source))
                        .await,
                    self.inherited_exec_policy_for_source(&state, Some(&session_source), &config)
                        .await,
                    client_mcp_extensions_override,
                )
            };
        let inherited_instructions = if let Some((parent, _)) = parent_context.as_ref() {
            Some(parent.session.inherited_instructions().await)
        } else if let Some(parent_thread_id) = parent_thread_id
            && let Ok(parent) = state.get_thread(parent_thread_id).await
        {
            Some(parent.session.inherited_instructions().await)
        } else {
            self.shared_thread_instructions_provider
                .get()
                .map(|provider| SessionInstructions {
                    thread_provider: Some(Arc::clone(provider)),
                    ..Default::default()
                })
        };
        // The caller pins the exact parent's lifecycle through publication. Capture its
        // instructions alongside authority rather than resolving a second parent generation.
        let residency_slot = self
            .reserve_v2_residency_slot(&state, &config, Some(thread_id))
            .await?;
        let notification_source = session_source.clone();
        let ownership_override = if self.current_agent_alias(thread_id).await?.is_some()
            && session_source.is_non_root_agent()
        {
            self.require_current_agent_ownership(thread_id).await?;
            Some(crate::session::AgentSessionOwnershipOverride {
                session_id: self.session_id(),
            })
        } else {
            None
        };
        match state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                ownership_override,
                registration: crate::thread_manager::ThreadRegistration::Deferred,
                config,
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections,
                inherited_environments,
                inherited_instructions,
                inherited_exec_policy,
                client_mcp_extensions_override,
            })
            .await
        {
            Ok(reloaded_thread) => {
                if reloaded_thread.runtime_origin
                    == crate::thread_manager::ThreadRuntimeOrigin::Existing
                {
                    return Err(CodexErr::Fatal(
                        "deferred agent restoration adopted an existing runtime".to_string(),
                    ));
                }
                let validation = self
                    .validate_loaded_v2_agent(&reloaded_thread.thread, Some(&notification_source))
                    .and_then(|_| {
                        self.validate_loaded_rollout_path(
                            &reloaded_thread.thread,
                            expected_rollout_path.as_deref(),
                        )
                    });
                if let Err(error) = validation {
                    return Err(self
                        .cleanup_unpublished_restoration(&reloaded_thread.thread, error)
                        .await);
                }
                residency_slot.commit(reloaded_thread.thread_id);
                Ok(reloaded_thread.thread)
            }
            Err(err) => Err(err),
        }
    }
}
