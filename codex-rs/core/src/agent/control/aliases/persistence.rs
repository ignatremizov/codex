use super::*;

impl LocalAgentControl {
    /// Persist a child alias and edge before publishing its runtime.
    ///
    /// The caller must hold the direct parent's lifecycle guard through this call so a
    /// concurrent subtree close cannot take its final membership snapshot first.
    pub(in crate::agent::control) async fn persist_thread_spawn_for_source(
        &self,
        child_thread: &crate::CodexThread,
        child_thread_id: ThreadId,
        session_source: Option<&SessionSource>,
        persistence: ThreadSpawnPersistence,
    ) -> CodexResult<PersistedAgentSpawn> {
        let Some(parent_thread_id) = session_source.and_then(SessionSource::parent_thread_id)
        else {
            return Ok(PersistedAgentSpawn::default());
        };
        if child_thread.config_snapshot().await.ephemeral {
            return Ok(PersistedAgentSpawn::default());
        }
        let Ok(state) = self.upgrade() else {
            return Ok(PersistedAgentSpawn::default());
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(PersistedAgentSpawn::default());
        };
        let session_id = match self.bound_session_id() {
            Some(session_id) if agent_graph_store.supports_agent_aliases() => session_id,
            Some(_) | None => {
                // Unbound controls and topology-only stores can retain the parent edge, but there
                // is no durable root-relative namespace in which an alias is authoritative.
                if let Err(err) = agent_graph_store
                    .upsert_thread_spawn_edge(
                        parent_thread_id,
                        child_thread_id,
                        ThreadSpawnEdgeStatus::Open,
                    )
                    .await
                {
                    warn!("failed to persist thread-spawn edge: {err}");
                }
                return Ok(PersistedAgentSpawn::default());
            }
        };

        let request = AllocateAgentAliasRequest {
            session_id,
            parent_thread_id,
            child_thread_id,
            nickname: self
                .get_agent_metadata(child_thread_id)
                .and_then(|metadata| metadata.agent_nickname)
                .or_else(|| session_source.and_then(SessionSource::get_nickname)),
            task_path: match &persistence {
                ThreadSpawnPersistence::New { task_path } => task_path.clone(),
                ThreadSpawnPersistence::Resume
                | ThreadSpawnPersistence::ControlledResume
                | ThreadSpawnPersistence::Transfer { .. } => None,
            },
        };
        let mut transfer = None;
        let alias = match persistence {
            ThreadSpawnPersistence::New { .. } => {
                agent_graph_store.allocate_agent_alias(request).await
            }
            ThreadSpawnPersistence::Resume | ThreadSpawnPersistence::ControlledResume => {
                agent_graph_store.activate_agent_alias(request).await
            }
            ThreadSpawnPersistence::Transfer {
                expected_previous_session_id,
                reserved_descendant_thread_ids,
                authored_selector,
                task_path,
            } => match reserved_descendant_thread_ids {
                Some(expected_descendant_thread_ids) => {
                    match agent_graph_store
                        .transfer_agent_alias(TransferAgentAliasRequest {
                            expected_previous_session_id,
                            expected_descendant_thread_ids,
                            new_session_id: session_id,
                            new_parent_thread_id: parent_thread_id,
                            thread_id: child_thread_id,
                            nickname: request.nickname.clone(),
                            authored_selector,
                            task_path,
                        })
                        .await
                    {
                        Ok(committed) => {
                            let alias = match &committed {
                                AgentAliasTransfer::AlreadyOwned { .. } => {
                                    agent_graph_store.activate_agent_alias(request).await
                                }
                                AgentAliasTransfer::Transferred { alias, .. } => Ok(alias.clone()),
                            };
                            transfer = Some(committed);
                            alias
                        }
                        Err(err) => Err(err),
                    }
                }
                None => Err(codex_agent_graph_store::AgentGraphStoreError::Internal {
                    message: format!(
                        "ownership transfer for {child_thread_id} reached persistence before its \
                         descendant rollout writers were reserved"
                    ),
                }),
            },
        }
        .map_err(|err| match err {
            codex_agent_graph_store::AgentGraphStoreError::InvalidRequest { message } => {
                CodexErr::InvalidRequest(message)
            }
            codex_agent_graph_store::AgentGraphStoreError::Internal { message } => {
                CodexErr::Fatal(format!(
                    "failed to persist durable alias for spawned agent {child_thread_id}: {message}"
                ))
            }
        })?;
        Ok(PersistedAgentSpawn {
            alias: Some(alias),
            transfer,
        })
    }

    pub(in crate::agent::control) async fn set_persisted_agent_lifecycle_state(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> CodexResult<bool> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(false);
        };
        let manager = self.upgrade()?;
        let Some(agent_graph_store) = manager.agent_graph_store() else {
            return Ok(false);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(false);
        }
        agent_graph_store
            .ensure_agent_alias_namespace(session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable agent aliases for {}: {err}",
                    session_id
                ))
            })?;
        agent_graph_store
            .set_agent_lifecycle_state(session_id, child_thread_id, status)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to persist agent lifecycle for {child_thread_id}: {err}"
                ))
            })
    }

    pub(in crate::agent::control) async fn persist_agent_closed(
        &self,
        child_thread_id: ThreadId,
    ) -> CodexResult<()> {
        if self
            .set_persisted_agent_lifecycle_state(child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await?
        {
            return Ok(());
        }
        let manager = self.upgrade()?;
        let Some(agent_graph_store) = manager.agent_graph_store() else {
            return Ok(());
        };
        if agent_graph_store.supports_agent_aliases() && self.bound_session_id().is_some() {
            // A failed owned update can mean ownership was transferred. Never fall back to an
            // unqualified edge mutation that could close the destination owner's runtime.
            return Err(CodexErr::InvalidRequest(format!(
                "agent {child_thread_id} is no longer controlled by this root"
            )));
        }
        agent_graph_store
            .set_thread_spawn_edge_status(child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to persist closed agent state for {child_thread_id}: {err}"
                ))
            })
    }
}
