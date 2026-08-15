use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentAliasState;
use codex_agent_graph_store::AllocateAgentAliasRequest;

use super::*;
use crate::agent::control::LiveAgentMetadataDisposition;
use crate::agent::control::agent_alias_lifecycle_status;
use crate::agent::control::setup_cleanup::SetupCleanupGuard;

struct MissingPersistedAgentEdge {
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
    nickname: Option<String>,
}

#[derive(Default)]
struct StoredAgentAncestry {
    parent_thread_id: Option<ThreadId>,
    source: Option<SessionSource>,
}

impl ThreadManager {
    async fn recover_unaliased_resume_owner(
        &self,
        resumed_thread_id: ThreadId,
        initial_history: &InitialHistory,
        agent_graph_store: &Arc<dyn AgentGraphStore>,
        stored_ancestry: &StoredAgentAncestry,
    ) -> CodexResult<Option<SessionId>> {
        let persisted_identity =
            initial_history
                .get_rollout_items()
                .iter()
                .rev()
                .find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line)
                        if meta_line.meta.id == resumed_thread_id =>
                    {
                        Some((meta_line.meta.session_id, meta_line.meta.source.clone()))
                    }
                    RolloutItem::SessionMeta(_)
                    | RolloutItem::ResponseItem(_)
                    | RolloutItem::Compacted(_)
                    | RolloutItem::InterAgentCommunication(_)
                    | RolloutItem::InterAgentCommunicationMetadata { .. }
                    | RolloutItem::AgentResponseObservation(_)
                    | RolloutItem::TurnContext(_)
                    | RolloutItem::WorldState(_)
                    | RolloutItem::SecurityRiskScore(_)
                    | RolloutItem::TokenUsageRecord(_)
                    | RolloutItem::RealtimeItem(_)
                    | RolloutItem::EventMsg(_) => None,
                });
        let mut ancestry_root = resumed_thread_id;
        let mut found_ancestor = false;
        let mut visited = HashSet::from([resumed_thread_id]);
        let persisted_source = persisted_identity
            .as_ref()
            .map(|(_, source)| source)
            .or(stored_ancestry.source.as_ref());
        let mut initial_parent_thread_id = initial_history
            .get_resumed_parent_thread_id()
            .or(stored_ancestry.parent_thread_id)
            .or_else(|| persisted_source.and_then(SessionSource::parent_thread_id));
        let mut missing_edges = Vec::new();

        loop {
            let graph_parent = agent_graph_store
                .find_thread_spawn_parent(ancestry_root)
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to resolve persisted ancestry for resumed thread \
                         {resumed_thread_id}: {err}"
                    ))
                })?;
            let parent_thread_id = match graph_parent {
                Some(parent_thread_id) => Some(parent_thread_id),
                None => {
                    let (parent_thread_id, nickname) = if ancestry_root == resumed_thread_id {
                        (
                            initial_parent_thread_id.take(),
                            persisted_source.and_then(SessionSource::get_nickname),
                        )
                    } else {
                        let stored = self
                            .state
                            .read_stored_thread(ReadThreadParams {
                                thread_id: ancestry_root,
                                include_archived: true,
                                include_history: false,
                            })
                            .await?;
                        (
                            stored
                                .parent_thread_id
                                .or_else(|| stored.source.parent_thread_id()),
                            stored
                                .agent_nickname
                                .or_else(|| stored.source.get_nickname()),
                        )
                    };
                    if let Some(parent_thread_id) = parent_thread_id {
                        missing_edges.push(MissingPersistedAgentEdge {
                            parent_thread_id,
                            child_thread_id: ancestry_root,
                            nickname,
                        });
                    }
                    parent_thread_id
                }
            };
            let Some(parent_thread_id) = parent_thread_id else {
                break;
            };
            if !visited.insert(parent_thread_id) {
                return Err(CodexErr::InvalidRequest(format!(
                    "resumed thread {resumed_thread_id} belongs to a cyclic persisted spawn graph"
                )));
            }
            ancestry_root = parent_thread_id;
            found_ancestor = true;
        }

        let namespace_session_id = if found_ancestor {
            Some(SessionId::from(ancestry_root))
        } else {
            // Older V1 children persisted their own thread UUID as session_id. That value is a
            // synthesized compatibility artifact, not a root ownership namespace.
            match persisted_identity.as_ref() {
                Some((persisted_session_id, persisted_source)) => (!persisted_source
                    .is_non_root_agent()
                    || *persisted_session_id != SessionId::from(resumed_thread_id))
                .then_some(*persisted_session_id),
                None if persisted_source.is_some_and(SessionSource::is_non_root_agent) => None,
                None => Some(SessionId::from(resumed_thread_id)),
            }
        };
        let Some(namespace_session_id) = namespace_session_id else {
            return Ok(None);
        };

        agent_graph_store
            .ensure_agent_alias_namespace(namespace_session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to initialize durable owner for resumed thread {resumed_thread_id}: \
                     {err}"
                ))
            })?;
        // Reconstruct only rollout-backed gaps. Existing graph edges retain their authoritative
        // lifecycle, while each activation transaction refuses conflicting current ownership.
        for missing_edge in missing_edges.into_iter().rev() {
            match agent_graph_store
                .find_agent_alias_by_thread(namespace_session_id, missing_edge.child_thread_id)
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to inspect recovered ancestry for resumed thread \
                         {resumed_thread_id}: {err}"
                    ))
                })? {
                Some(alias) if alias.state == AgentAliasState::Transferred => {
                    return Err(CodexErr::InvalidRequest(format!(
                        "persisted ancestor {} was transferred out of root {}",
                        missing_edge.child_thread_id, namespace_session_id
                    )));
                }
                Some(_) => {}
                None => {
                    agent_graph_store
                        .activate_agent_alias(AllocateAgentAliasRequest {
                            session_id: namespace_session_id,
                            parent_thread_id: missing_edge.parent_thread_id,
                            child_thread_id: missing_edge.child_thread_id,
                            nickname: missing_edge.nickname,
                        })
                        .await
                        .map_err(|err| {
                            CodexErr::Fatal(format!(
                                "failed to recover durable ancestry for resumed thread \
                                 {resumed_thread_id}: {err}"
                            ))
                        })?;
                }
            }
        }
        agent_graph_store
            .ensure_agent_alias_namespace(namespace_session_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to finish durable ancestry recovery for resumed thread \
                     {resumed_thread_id}: {err}"
                ))
            })?;
        Ok(Some(namespace_session_id))
    }

    async fn ensure_agent_alias_owner_from_history(
        &self,
        resumed_thread_id: ThreadId,
        initial_history: &InitialHistory,
        agent_graph_store: &Arc<dyn AgentGraphStore>,
        stored_ancestry: &StoredAgentAncestry,
    ) -> CodexResult<Option<SessionId>> {
        if let Some(alias) = self.state.current_agent_alias(resumed_thread_id).await? {
            return Ok(Some(alias.session_id));
        }
        let recovered_session_id = match self
            .recover_unaliased_resume_owner(
                resumed_thread_id,
                initial_history,
                agent_graph_store,
                stored_ancestry,
            )
            .await
        {
            Ok(session_id) => session_id,
            Err(err) => {
                if let Some(alias) = self.state.current_agent_alias(resumed_thread_id).await? {
                    return Ok(Some(alias.session_id));
                }
                return Err(err);
            }
        };
        let current_alias = self.state.current_agent_alias(resumed_thread_id).await?;
        if recovered_session_id.is_some() && current_alias.is_none() {
            return Err(CodexErr::Fatal(format!(
                "failed to recover durable owner for resumed thread {resumed_thread_id} from its \
                 persisted spawn ancestry"
            )));
        }
        Ok(current_alias.map(|alias| alias.session_id))
    }

    /// Ensure the durable alias namespace and ownership chain for a stored thread.
    pub async fn ensure_agent_alias_namespace_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<SessionId> {
        let agent_graph_store = self.state.agent_graph_store().ok_or_else(|| {
            CodexErr::UnsupportedOperation("durable agent aliases are unavailable".to_string())
        })?;
        if !agent_graph_store.supports_agent_aliases() {
            return Err(CodexErr::UnsupportedOperation(
                "durable agent aliases are unavailable".to_string(),
            ));
        }
        if let Some(alias) = self.state.current_agent_alias(thread_id).await? {
            return Ok(alias.session_id);
        }

        let stored_thread = self
            .state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let stored_ancestry = StoredAgentAncestry {
            parent_thread_id: stored_thread.parent_thread_id,
            source: Some(stored_thread.source.clone()),
        };
        let history = self
            .state
            .load_agent_model_context(thread_id, stored_thread.history_mode)
            .await?
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        self.ensure_agent_alias_owner_from_history(
            thread_id,
            &initial_history,
            &agent_graph_store,
            &stored_ancestry,
        )
        .await?
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "failed to recover the owning root for agent {thread_id}"
            ))
        })
    }

    pub(super) async fn resume_thread_with_current_owner(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<NewThread> {
        let resumed_thread_id = match &initial_history {
            InitialHistory::Resumed(resumed) => Some(resumed.conversation_id),
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        if let Some(resumed_thread_id) = resumed_thread_id
            && let Some(agent_graph_store) = self.state.agent_graph_store()
            && agent_graph_store.supports_agent_aliases()
        {
            self.ensure_agent_alias_owner_from_history(
                resumed_thread_id,
                &initial_history,
                &agent_graph_store,
                &StoredAgentAncestry::default(),
            )
            .await?;
        }
        let (current_alias, _resume_lifecycle_guards) =
            if let Some(resumed_thread_id) = resumed_thread_id {
                // Serialize cold resumes through the durable owner, then match the graph's
                // parent-before-child lifecycle order. Ownership transfer can change both values
                // before these guards are acquired, so commit to the snapshot only after reading
                // it back under all three boundaries.
                loop {
                    let expected_alias = self.state.current_agent_alias(resumed_thread_id).await?;
                    let expected_owner = expected_alias.as_ref().map(|alias| alias.session_id);
                    let expected_parent = if expected_owner.is_some() {
                        self.state.agent_parent(resumed_thread_id).await?
                    } else {
                        None
                    };
                    if let Some(owner) = expected_owner
                        && ThreadId::from(owner) != resumed_thread_id
                        && expected_parent.is_none()
                    {
                        return Err(CodexErr::Fatal(format!(
                            "durably owned resumed thread {resumed_thread_id} has no parent edge"
                        )));
                    }
                    let mut lifecycle_thread_ids = Vec::new();
                    if let Some(owner) = expected_owner {
                        lifecycle_thread_ids.push(ThreadId::from(owner));
                    }
                    if let Some(parent_thread_id) = expected_parent
                        && !lifecycle_thread_ids.contains(&parent_thread_id)
                    {
                        lifecycle_thread_ids.push(parent_thread_id);
                    }
                    if !lifecycle_thread_ids.contains(&resumed_thread_id) {
                        lifecycle_thread_ids.push(resumed_thread_id);
                    }
                    let mut lifecycle_guards = Vec::with_capacity(lifecycle_thread_ids.len());
                    for thread_id in lifecycle_thread_ids {
                        lifecycle_guards.push(
                            self.state
                                .agent_lifecycle_lock(thread_id)
                                .lock_owned()
                                .await,
                        );
                    }
                    let current_alias = self.state.current_agent_alias(resumed_thread_id).await?;
                    let current_owner = current_alias.as_ref().map(|alias| alias.session_id);
                    let current_parent = if current_owner.is_some() {
                        self.state.agent_parent(resumed_thread_id).await?
                    } else {
                        None
                    };
                    if let Some(owner) = current_owner
                        && ThreadId::from(owner) != resumed_thread_id
                        && current_parent.is_none()
                    {
                        return Err(CodexErr::Fatal(format!(
                            "durably owned resumed thread {resumed_thread_id} has no parent edge"
                        )));
                    }
                    if (current_owner, current_parent) == (expected_owner, expected_parent) {
                        break (current_alias, lifecycle_guards);
                    }
                }
            } else {
                (None, Vec::new())
            };
        let (mut session_source, thread_source) = initial_history
            .get_resumed_session_sources()
            .unwrap_or_else(|| (self.state.session_source.clone(), None));
        let controlled_owner = current_alias.as_ref().map(|alias| alias.session_id);
        let loaded_agent_control = if let Some(owner) = controlled_owner {
            let threads = self.state.threads.read().await;
            threads
                .get(&ThreadId::from(owner))
                .filter(|root| root.session.session_id() == owner)
                .or_else(|| {
                    threads
                        .values()
                        .find(|thread| thread.session.session_id() == owner)
                })
                .map(|thread| thread.session.services.agent_control.clone())
        } else {
            None
        };
        let agent_control = match controlled_owner {
            Some(owner) => match loaded_agent_control {
                Some(agent_control) => agent_control,
                None => self.agent_control_for_config(&config).with_session_id(
                    owner,
                    config
                        .effective_agent_max_threads(MultiAgentVersion::V2)
                        .unwrap_or(usize::MAX),
                ),
            },
            None => self.agent_control_for_config(&config),
        };
        // A standalone resume has no source root to adopt into. Reopen a durably owned rollout
        // through its current owner instead of reconstructing the historical controller embedded
        // in an older V1 rollout. Session startup acquires the exclusive writer; alias activation
        // and runtime publication happen only after that succeeds.
        if let Some(resumed_thread_id) = resumed_thread_id
            && controlled_owner.is_some()
        {
            session_source = agent_control
                .canonical_controlled_resume_source(resumed_thread_id, session_source)
                .await?;
        }
        if let InitialHistory::Resumed(resumed) = &initial_history
            && initial_history.get_multi_agent_version() == Some(MultiAgentVersion::V2)
            && !session_source.is_non_root_agent()
        {
            agent_control
                .restore_v2_agent_metadata(&config, resumed.conversation_id)
                .await;
        }
        let activates_controlled_alias =
            controlled_owner.is_some() && session_source.parent_thread_id().is_some();
        let mut controlled_registration = if activates_controlled_alias {
            let thread_id = resumed_thread_id.ok_or_else(|| {
                CodexErr::Fatal("controlled resume is missing a thread ID".to_string())
            })?;
            agent_control
                .reserve_controlled_resume_registration(&config, thread_id, &session_source)
                .await?
        } else {
            None
        };
        let options = StartThreadOptions {
            initial_history,
            session_source: Some(session_source.clone()),
            thread_source,
            parent_trace,
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        let mut request = ThreadSpawnRequest::new(options, auth_manager, agent_control.clone());
        if activates_controlled_alias {
            request.runtime_publication = ThreadRuntimePublication::Deferred;
        }
        let resumed = Box::pin(self.state.spawn_thread(request)).await?;
        let mut setup_cleanup = (activates_controlled_alias
            && resumed.runtime_origin == ThreadRuntimeOrigin::Created)
            .then(|| {
                SetupCleanupGuard::new_with_agent_lifecycle(
                    "controlled thread resume",
                    Arc::clone(&self.state),
                    resumed.thread_id,
                    {
                        let agent_control = agent_control.clone();
                        let state = Arc::clone(&self.state);
                        let thread = Arc::clone(&resumed.thread);
                        let previous_alias = current_alias.clone();
                        let agent_graph_store = self.state.agent_graph_store();
                        async move {
                            let owns_runtime =
                                state.thread_instance_is_current_or_pending(&thread).await;
                            let lifecycle_restore = if owns_runtime {
                                match (agent_graph_store, previous_alias) {
                                    (Some(agent_graph_store), Some(previous_alias))
                                        if agent_graph_store.supports_agent_aliases() =>
                                    {
                                        let status =
                                            agent_alias_lifecycle_status(previous_alias.state);
                                        match status {
                                            Some(status) => agent_graph_store
                                                .set_agent_lifecycle_state(
                                                    previous_alias.session_id,
                                                    previous_alias.thread_id,
                                                    status,
                                                )
                                                .await
                                                .map_err(|err| {
                                                    CodexErr::Fatal(format!(
                                                        "failed to restore controlled-resume lifecycle: {err}"
                                                    ))
                                                })
                                                .and_then(|restored| {
                                                    if restored {
                                                        Ok(())
                                                    } else {
                                                        Err(CodexErr::Fatal(format!(
                                                            "controlled-resume lifecycle disappeared for {}",
                                                            previous_alias.thread_id
                                                        )))
                                                    }
                                                }),
                                            None => Err(CodexErr::Fatal(format!(
                                                "cannot restore transferred historical alias for {}",
                                                previous_alias.thread_id
                                            ))),
                                        }
                                    }
                                    (Some(_) | None, Some(_) | None) => Ok(()),
                                }
                            } else {
                                Ok(())
                            };
                            let runtime_cleanup = agent_control
                                .discard_unpublished_agent_instance(
                                    &thread,
                                    LiveAgentMetadataDisposition::Release,
                                )
                                .await;
                            lifecycle_restore.and(runtime_cleanup)
                        }
                    },
                )
            });
        if let Some(owner) = controlled_owner
            && resumed.thread.session.services.agent_control.session_id() != owner
        {
            if let Some(setup_cleanup) = setup_cleanup.take() {
                setup_cleanup.rollback().await?;
            }
            return Err(CodexErr::InvalidRequest(format!(
                "thread {} is already live under a different root",
                resumed.thread_id
            )));
        }
        if activates_controlled_alias {
            let setup_result: CodexResult<()> = match agent_control
                .activate_controlled_resume_alias(resumed.thread.as_ref(), &session_source)
                .await
            {
                Ok(_) => {
                    let registration_commit = controlled_registration
                        .take()
                        .map(|registration| registration.commit());
                    match self.state.publish_thread(&resumed.thread).await {
                        Ok(()) => {
                            if let Some(registration_commit) = registration_commit {
                                registration_commit.publish();
                            }
                            Ok(())
                        }
                        Err(err) => Err(err),
                    }
                }
                Err(err) => Err(err),
            };
            if let Err(err) = setup_result {
                if let Some(setup_cleanup) = setup_cleanup.take()
                    && let Err(cleanup_err) = setup_cleanup.rollback().await
                {
                    return Err(CodexErr::Fatal(format!(
                        "{err}; failed to discard rejected resume {}: {cleanup_err}",
                        resumed.thread_id
                    )));
                }
                return Err(err);
            }
        }
        if let Some(setup_cleanup) = setup_cleanup.take() {
            setup_cleanup.disarm();
        }
        Ok(resumed.into_new_thread())
    }
}

impl ThreadManagerState {
    async fn current_agent_alias(&self, thread_id: ThreadId) -> CodexResult<Option<AgentAlias>> {
        let Some(agent_graph_store) = self.agent_graph_store() else {
            return Ok(None);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(None);
        }
        agent_graph_store
            .find_current_agent_alias_by_thread(thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to resolve current owner for resumed thread {thread_id}: {err}"
                ))
            })
    }

    async fn agent_parent(&self, thread_id: ThreadId) -> CodexResult<Option<ThreadId>> {
        let Some(agent_graph_store) = self.agent_graph_store() else {
            return Ok(None);
        };
        agent_graph_store
            .find_thread_spawn_parent(thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to resolve current parent for resumed thread {thread_id}: {err}"
                ))
            })
    }
}
