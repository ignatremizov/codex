//! Restore recorded agent runtimes without replacing their owning control plane.

use super::residency::is_v2_resident_session_source;
use super::restore_environments::explicit_workspace_environments;
use super::restore_metadata::apply_restored_agent_model;
use super::restore_metadata::apply_restored_v2_agent_role;
use super::resume_role::apply_resumed_agent_role;
use super::spawn::load_agent_model_context;
use super::user_resume::RestoredAgent;
use super::user_resume::ResumeAuthority;
use super::*;
use crate::codex_thread::CodexThread;
use codex_protocol::mcp::ClientMcpExtensions;

impl LocalAgentControl {
    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let session_source = if self.bound_session_id().is_some() {
            self.canonical_controlled_resume_source(thread_id, session_source)
                .await?
        } else {
            session_source
        };
        let root_depth = thread_spawn_depth(&session_source).unwrap_or(0);
        let restored = Box::pin(self.resume_single_agent_from_rollout(
            config.clone(),
            thread_id,
            session_source,
            /*initial_history_override*/ None,
            /*client_mcp_extensions_override*/ None,
        ))
        .await?;
        let resumed_thread = restored.thread;
        let resumed_multi_agent_version = restored.version;
        let resumed_thread_id = resumed_thread.session.thread_id();
        self.restore_open_agent_descendants(
            &config,
            thread_id,
            root_depth,
            resumed_multi_agent_version,
        )
        .await?;
        Ok(resumed_thread_id)
    }

    pub(super) async fn restore_open_agent_descendants(
        &self,
        config: &Config,
        thread_id: ThreadId,
        root_depth: i32,
        resumed_multi_agent_version: MultiAgentVersion,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        if config.multi_agent_version_from_features() == MultiAgentVersion::V2
            || resumed_multi_agent_version == MultiAgentVersion::V2
        {
            return Ok(());
        }
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(());
        };

        let mut resume_queue = VecDeque::from([(thread_id, root_depth)]);
        let mut visited = HashSet::from([thread_id]);
        while let Some((parent_thread_id, parent_depth)) = resume_queue.pop_front() {
            let child_ids = match agent_graph_store
                .list_thread_spawn_children(
                    parent_thread_id,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
            {
                Ok(child_ids) => child_ids,
                Err(err) => {
                    warn!(
                        "failed to load persisted thread-spawn children for {parent_thread_id}: {err}"
                    );
                    continue;
                }
            };

            for child_thread_id in child_ids {
                if !visited.insert(child_thread_id) {
                    return Err(CodexErr::InvalidRequest(
                        "cyclic persisted agent subtree".into(),
                    ));
                }
                let child_depth = parent_depth
                    .checked_add(1)
                    .ok_or_else(|| CodexErr::InvalidRequest("agent depth overflow".into()))?;
                let child_resumed = if state.get_thread(child_thread_id).await.is_ok() {
                    true
                } else {
                    let child_session_source =
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            depth: child_depth,
                            agent_path: None,
                            agent_nickname: None,
                            agent_role: None,
                        });
                    match Box::pin(self.resume_single_agent_from_rollout(
                        config.clone(),
                        child_thread_id,
                        child_session_source,
                        /*initial_history_override*/ None,
                        /*client_mcp_extensions_override*/ None,
                    ))
                    .await
                    {
                        Ok(_) => true,
                        Err(err) => {
                            warn!("failed to resume descendant thread {child_thread_id}: {err}");
                            false
                        }
                    }
                };
                if child_resumed {
                    resume_queue.push_back((child_thread_id, child_depth));
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn resume_v2_agent_from_history(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        initial_history: InitialHistory,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<Arc<CodexThread>> {
        let authority = if self.current_agent_alias(thread_id).await?.is_some() {
            ResumeAuthority::Controlled
        } else {
            ResumeAuthority::Recorded
        };
        let restored = Box::pin(self.resume_single_agent_from_rollout_unlocked(
            config,
            thread_id,
            session_source,
            Some(initial_history),
            Some(client_mcp_extensions),
            authority,
        ))
        .await?;
        if restored.version != MultiAgentVersion::V2 {
            return Err(CodexErr::InvalidRequest(format!(
                "persisted spawned child {thread_id} is not running Multi-Agent V2"
            )));
        }
        Ok(restored.thread)
    }

    async fn resume_single_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        initial_history_override: Option<InitialHistory>,
        client_mcp_extensions_override: Option<ClientMcpExtensions>,
    ) -> CodexResult<RestoredAgent> {
        let control = self.clone();
        tokio::spawn(async move {
            let state = control.upgrade()?;
            let lock = state.v2_spawn_resume_lock(thread_id);
            let _guard = lock.lock_owned().await;
            let authority = if control.bound_session_id().is_some()
                && control.current_agent_alias(thread_id).await?.is_some()
            {
                ResumeAuthority::ModelControlled
            } else {
                ResumeAuthority::Recorded
            };
            control
                .resume_single_agent_from_rollout_unlocked(
                    config,
                    thread_id,
                    session_source,
                    initial_history_override,
                    client_mcp_extensions_override,
                    authority,
                )
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent restoration worker failed: {error}")))?
    }

    pub(super) async fn resume_single_agent_from_rollout_unlocked(
        &self,
        mut config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        initial_history_override: Option<InitialHistory>,
        client_mcp_extensions_override: Option<ClientMcpExtensions>,
        authority: ResumeAuthority,
    ) -> CodexResult<RestoredAgent> {
        let state = self.upgrade()?;
        state.check_restoration_fence(thread_id)?;
        if state.get_thread(thread_id).await.is_ok() {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {thread_id} is already loaded; use its current runtime"
            )));
        }
        if matches!(
            authority,
            ResumeAuthority::Controlled | ResumeAuthority::ModelControlled
        ) {
            self.require_current_agent_ownership(thread_id).await?;
        }
        self.sync_durable_agent_nickname_reservations().await?;
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
        let (
            resumed_agent_path,
            resumed_agent_nickname,
            resumed_agent_role,
            stored_source,
            stored_parent_thread_id,
            initial_history,
        ) = match initial_history_override {
            Some(initial_history) => {
                let stored_source = initial_history
                    .get_resumed_session_sources()
                    .map(|(session_source, _)| session_source)
                    .unwrap_or_else(|| session_source.clone());
                let stored_parent_thread_id = initial_history
                    .get_resumed_parent_thread_id()
                    .or_else(|| stored_source.parent_thread_id());
                (
                    None,
                    None,
                    None,
                    stored_source,
                    stored_parent_thread_id,
                    initial_history,
                )
            }
            None => {
                let resumed_agent_path = stored_thread
                    .agent_path
                    .as_deref()
                    .map(AgentPath::try_from)
                    .transpose()
                    .map_err(|err| {
                        CodexErr::InvalidRequest(format!("invalid stored agent path: {err}"))
                    })?;
                let history =
                    load_agent_model_context(&state, thread_id, stored_thread.history_mode)
                        .await?
                        .ok_or(CodexErr::ThreadNotFound(thread_id))?;
                (
                    resumed_agent_path,
                    stored_thread.agent_nickname,
                    stored_thread.agent_role,
                    stored_thread.source,
                    stored_thread.parent_thread_id,
                    InitialHistory::Resumed(ResumedHistory {
                        conversation_id: thread_id,
                        history: Arc::new(history),
                        rollout_path: stored_thread.rollout_path,
                    }),
                )
            }
        };
        let canonical_session_source = initial_history
            .get_resumed_session_sources()
            .map(|(session_source, _)| session_source);
        let persisted_session_source = canonical_session_source
            .clone()
            .unwrap_or_else(|| stored_source.clone());
        let expected_rollout_path = match &initial_history {
            InitialHistory::Resumed(resumed) => resumed.rollout_path.clone(),
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &initial_history,
                Some(&session_source),
                stored_parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        if multi_agent_version == MultiAgentVersion::V1
            && let Some(role_name) = resumed_agent_role.as_deref()
        {
            // V1 resumes retain the caller's model selection, unlike V2's stored-model
            // precedence. Reapply role restrictions without changing that contract.
            let model_settings = (
                config.model.clone(),
                config.model_reasoning_effort.clone(),
                config.model_reasoning_summary,
            );
            apply_resumed_agent_role(&mut config, role_name).await?;
            (
                config.model,
                config.model_reasoning_effort,
                config.model_reasoning_summary,
            ) = model_settings;
        }
        if initial_history.get_multi_agent_version() == Some(MultiAgentVersion::V2)
            && multi_agent_version != MultiAgentVersion::V2
        {
            return Err(CodexErr::InvalidRequest(format!(
                "cannot restore persisted V2 child {thread_id} while agents are disabled"
            )));
        }
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let resume_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && is_v2_resident_session_source(&session_source);
        let reservation_max_threads = if resume_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = self.state.reserve_spawn_slot(reservation_max_threads)?;
        let (session_source, agent_metadata, register_resumed_agent) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                agent_nickname,
            }) => {
                if !matches!(authority, ResumeAuthority::Recorded) {
                    let agent_nickname = if matches!(authority, ResumeAuthority::Transfer { .. }) {
                        self.find_session_agent_alias(thread_id)
                            .await?
                            .and_then(|alias| alias.nickname)
                            .or(agent_nickname)
                    } else {
                        agent_nickname
                    };
                    let (source, metadata) = self.prepare_thread_spawn(
                        &mut reservation,
                        &config,
                        parent_thread_id,
                        depth,
                        agent_path,
                        agent_role.or(resumed_agent_role),
                        agent_nickname,
                    )?;
                    (source, metadata, true)
                } else if let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    agent_path,
                    agent_role,
                    agent_nickname,
                    ..
                })) = canonical_session_source
                {
                    let metadata = self.prepare_restored_agent_metadata_exact(
                        &mut reservation,
                        agent_path.clone(),
                        agent_role.clone(),
                        agent_nickname.clone(),
                    )?;
                    (
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            depth,
                            agent_path,
                            agent_nickname,
                            agent_role,
                        }),
                        metadata,
                        true,
                    )
                } else if stored_parent_thread_id.is_some()
                    || resumed_agent_path.is_some()
                    || resumed_agent_role.is_some()
                    || resumed_agent_nickname.is_some()
                {
                    let (session_source, metadata) = self.prepare_thread_spawn(
                        &mut reservation,
                        &config,
                        parent_thread_id,
                        depth,
                        agent_path.or(resumed_agent_path),
                        agent_role.or(resumed_agent_role),
                        agent_nickname.or(resumed_agent_nickname),
                    )?;
                    (session_source, metadata, true)
                } else {
                    (persisted_session_source, AgentMetadata::default(), false)
                }
            }
            other => (other, AgentMetadata::default(), false),
        };
        if resume_uses_v2_residency {
            apply_restored_v2_agent_role(&mut config, &session_source).await?;
            if config.multi_agent_version_override() == Some(MultiAgentVersion::Disabled) {
                return Err(CodexErr::InvalidRequest(format!(
                    "cannot restore persisted V2 child {thread_id} because its restored role disables agents"
                )));
            }
        }
        // Routing settings belong to the persisted child for every authorized restore path;
        // ownership and live runtime permissions are resolved independently above.
        apply_restored_agent_model(&mut config, stored_model, stored_model_provider)?;
        config.model_reasoning_effort = stored_reasoning_effort;
        config.service_tier = stored_service_tier;
        let parent = match session_source.parent_thread_id() {
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
        let residency_slot = if resume_uses_v2_residency {
            Some(
                self.reserve_v2_residency_slot(&state, &config, Some(thread_id))
                    .await?,
            )
        } else {
            None
        };
        let notification_source = session_source.clone();
        let parent_thread_id = session_source
            .parent_thread_id()
            .or_else(|| initial_history.get_resumed_parent_thread_id())
            .or(stored_parent_thread_id);
        let inherited_environments = match &parent {
            Some(parent) => {
                let captured = parent.session.services.turn_environments.snapshot().await;
                Some(if config.workspace_roots_explicit {
                    explicit_workspace_environments(&config, &captured)?
                } else {
                    captured
                })
            }
            None => None,
        };
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, Some(&session_source), &config)
            .await;
        let inherited_instructions = match &parent {
            Some(parent) => Some(parent.session.inherited_instructions().await),
            None => None,
        };

        let resumed_thread = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                registration: crate::thread_manager::ThreadRegistration::Deferred,
                ownership_override: (!matches!(authority, ResumeAuthority::Recorded)
                    && session_source.is_non_root_agent())
                .then_some(crate::session::AgentSessionOwnershipOverride {
                    session_id: self.session_id(),
                }),
                config: config.clone(),
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections: None,
                inherited_environments,
                inherited_instructions,
                inherited_exec_policy,
                client_mcp_extensions_override,
            })
            .await?;
        if resumed_thread.runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Existing {
            // Failure cleanup below owns only a runtime allocated by this restoration.
            // Never tear down an independently admitted runtime if that contract changes.
            return Err(CodexErr::Fatal(
                "deferred agent restoration adopted an existing runtime".to_string(),
            ));
        }
        let validation = if multi_agent_version == MultiAgentVersion::V2 {
            self.validate_loaded_v2_agent(&resumed_thread.thread, Some(&notification_source))
                .and_then(|_| {
                    self.validate_loaded_rollout_path(
                        &resumed_thread.thread,
                        expected_rollout_path.as_deref(),
                    )
                })
        } else {
            Ok(())
        };
        if let Err(error) = validation {
            return Err(self
                .cleanup_unpublished_restoration(&resumed_thread.thread, error)
                .await);
        }
        let mut agent_metadata = agent_metadata;
        agent_metadata.agent_id = Some(resumed_thread.thread_id);
        let registration_metadata = agent_metadata.clone();
        let transferred_descendants = match &authority {
            ResumeAuthority::Transfer { descendants, .. } => descendants.clone(),
            ResumeAuthority::Recorded
            | ResumeAuthority::ModelControlled
            | ResumeAuthority::Controlled => Vec::new(),
        };
        let persistence = match authority {
            ResumeAuthority::Recorded
            | ResumeAuthority::ModelControlled
            | ResumeAuthority::Controlled => {
                self.current_agent_alias(thread_id).await.map(|alias| {
                    super::aliases::PersistedAgentSpawn {
                        alias,
                        transfer: None,
                    }
                })
            }
            transfer @ ResumeAuthority::Transfer { .. } => {
                self.persist_thread_spawn_for_source(
                    resumed_thread.thread.as_ref(),
                    thread_id,
                    Some(&notification_source),
                    transfer.persistence(),
                )
                .await
            }
        };
        let persisted = match persistence {
            Ok(persisted) => persisted,
            Err(error) => {
                return Err(self
                    .cleanup_unpublished_restoration(&resumed_thread.thread, error)
                    .await);
            }
        };
        if matches!(
            persisted.transfer,
            Some(codex_agent_graph_store::AgentAliasTransfer::Transferred { .. })
        ) {
            // Revoke future old-owner observations before publishing the new runtime.
            // Accepted exact-instance receipts retain their independent capability.
            for id in std::iter::once(thread_id).chain(transferred_descendants) {
                state.advance_agent_lifecycle_generation(id);
            }
        }
        if let Err(error) = self
            .publish_restored_agent(&resumed_thread.thread, parent.as_ref(), || {
                if register_resumed_agent && !reservation.commit_if_absent(registration_metadata) {
                    return Err(CodexErr::InvalidRequest(
                        "restored agent registration changed while loading".to_string(),
                    ));
                }
                Ok(())
            })
            .await
        {
            let error = self
                .cleanup_unpublished_restoration(&resumed_thread.thread, error)
                .await;
            if matches!(
                persisted.transfer,
                Some(codex_agent_graph_store::AgentAliasTransfer::Transferred { .. })
            ) {
                return Ok(RestoredAgent {
                    thread: resumed_thread.thread,
                    version: multi_agent_version,
                    persisted,
                    post_commit_warning: Some(format!(
                        "ownership transferred, but runtime publication failed: {error}"
                    )),
                });
            }
            return Err(error);
        }
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(resumed_thread.thread_id);
        }
        if multi_agent_version != MultiAgentVersion::V2
            && state
                .agent_graph_store()
                .is_some_and(|graph| !graph.supports_agent_aliases())
        {
            self.persist_thread_spawn_for_source(
                resumed_thread.thread.as_ref(),
                thread_id,
                Some(&notification_source),
                super::aliases::ThreadSpawnPersistence::Resume,
            )
            .await?;
        }
        Ok(RestoredAgent {
            thread: resumed_thread.thread,
            version: multi_agent_version,
            persisted,
            post_commit_warning: None,
        })
    }
}
