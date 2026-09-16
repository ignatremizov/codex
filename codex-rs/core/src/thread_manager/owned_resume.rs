use codex_agent_graph_store::AgentAlias;
use codex_agent_graph_store::AgentAliasState;
use codex_agent_graph_store::AllocateAgentAliasRequest;

use super::*;

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
                    | RolloutItem::RetainedContext(_)
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

    /// Reload an explicit live revert, restoring only its send policy before V1 publication.
    ///
    /// Ordinary resume and history fork never take a live permission handoff.
    #[allow(clippy::too_many_arguments)]
    pub async fn resume_thread_after_live_revert(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
        client_mcp_extensions: ClientMcpExtensions,
        messaging: Option<crate::LiveRevertMessagingSnapshot>,
    ) -> CodexResult<NewThread> {
        match messaging {
            Some(messaging) => {
                self.resume_thread_with_current_owner(
                    config,
                    initial_history,
                    auth_manager,
                    parent_trace,
                    client_mcp_extensions,
                    Some(messaging),
                )
                .await
            }
            None => {
                self.resume_thread_with_history(
                    config,
                    initial_history,
                    auth_manager,
                    parent_trace,
                    client_mcp_extensions,
                )
                .await
            }
        }
    }

    pub(super) async fn resume_thread_with_current_owner(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
        client_mcp_extensions: ClientMcpExtensions,
        live_revert_messaging: Option<crate::LiveRevertMessagingSnapshot>,
    ) -> CodexResult<NewThread> {
        let manager = Self {
            state: Arc::clone(&self.state),
            _test_codex_home_guard: None,
        };
        tokio::spawn(async move {
            manager
                .resume_owned_thread(
                    config,
                    initial_history,
                    auth_manager,
                    parent_trace,
                    client_mcp_extensions,
                    live_revert_messaging,
                )
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("owned resume worker failed: {error}")))?
    }

    async fn resume_owned_thread(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
        client_mcp_extensions: ClientMcpExtensions,
        live_revert_messaging: Option<crate::LiveRevertMessagingSnapshot>,
    ) -> CodexResult<NewThread> {
        let thread_id = match &initial_history {
            InitialHistory::Resumed(history) => Some(history.conversation_id),
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        let _target_guard = match thread_id {
            Some(id) => {
                let guard = self.state.agent_lifecycle_lock(id).lock_owned().await;
                self.state.ensure_membership_mutation_allowed(id).await?;
                Some(guard)
            }
            None => None,
        };
        if let Some(id) = thread_id
            && let Some(graph) = self
                .state
                .agent_graph_store()
                .filter(|graph| graph.supports_agent_aliases())
        {
            self.ensure_agent_alias_owner_from_history(
                id,
                &initial_history,
                &graph,
                &StoredAgentAncestry::default(),
            )
            .await?;
        }
        let alias = match thread_id {
            Some(id) => {
                self.state.check_restoration_fence(id)?;
                self.state.current_agent_alias(id).await?
            }
            None => None,
        };
        let (mut source, thread_source) = initial_history
            .get_resumed_session_sources()
            .unwrap_or_else(|| (self.state.session_source.clone(), None));
        let owner_id = alias.as_ref().map(|alias| ThreadId::from(alias.session_id));
        let _owner_guard = match owner_id.filter(|owner| Some(*owner) != thread_id) {
            Some(owner) => Some(
                self.state
                    .agent_lifecycle_lock(owner)
                    .try_lock_owned()
                    .map_err(|_| {
                        CodexErr::InvalidRequest(
                            "owning root is busy; retry its resume first".into(),
                        )
                    })?,
            ),
            None => None,
        };
        if let Some(owner) = owner_id {
            self.state.ensure_membership_mutation_allowed(owner).await?;
        }
        let control = if let Some(messaging) = &live_revert_messaging {
            let control = messaging.control_for_resume(thread_id.ok_or_else(|| {
                CodexErr::InvalidRequest("live revert requires resumed history".into())
            })?)?;
            if alias
                .as_ref()
                .is_some_and(|alias| alias.session_id != control.session_id())
            {
                return Err(CodexErr::InvalidRequest("live revert owner changed".into()));
            }
            control
        } else if let Some(alias) = &alias {
            let threads = self.state.threads.read().await;
            threads
                .get(&ThreadId::from(alias.session_id))
                .filter(|thread| {
                    thread.session.services.agent_control.bound_session_id()
                        == Some(alias.session_id)
                })
                .or_else(|| {
                    threads.values().find(|thread| {
                        thread.session.services.agent_control.bound_session_id()
                            == Some(alias.session_id)
                    })
                })
                .map(|thread| thread.session.services.agent_control.clone())
                .unwrap_or_else(|| {
                    self.agent_control_for_config(&config).with_session_id(
                        alias.session_id,
                        config
                            .effective_agent_max_threads(MultiAgentVersion::V2)
                            .unwrap_or(usize::MAX),
                    )
                })
        } else {
            self.agent_control_for_config(&config)
        };
        if let Some(id) = thread_id
            && alias.is_some()
        {
            source = control
                .canonical_controlled_resume_source(id, source)
                .await?;
        }
        let controlled_child = alias.is_some() && source.parent_thread_id().is_some();
        let parent = match source.parent_thread_id() {
            Some(id) => self.state.get_thread(id).await.ok(),
            None => None,
        };
        if controlled_child
            && initial_history.get_multi_agent_version() == Some(MultiAgentVersion::V2)
            && parent.is_none()
        {
            return Err(CodexErr::InvalidRequest(
                "resume the owning parent before its V2 child".into(),
            ));
        }
        let _parent_guard = match &parent {
            Some(parent) if Some(parent.session.thread_id()) != owner_id => Some(
                self.state
                    .agent_lifecycle_lock(parent.session.thread_id())
                    .try_lock_owned()
                    .map_err(|_| {
                        CodexErr::InvalidRequest(
                            "resume owner is busy; retry after its lifecycle operation".into(),
                        )
                    })?,
            ),
            Some(_) | None => None,
        };
        if let Some(id) = thread_id
            && let Ok(thread) = self.state.get_thread(id).await
        {
            if live_revert_messaging.is_some() {
                return Err(CodexErr::InvalidRequest(
                    "live revert cannot adopt an already published runtime".into(),
                ));
            }
            if alias.as_ref().is_some_and(|alias| {
                thread.session.services.agent_control.session_id() != alias.session_id
            }) {
                return Err(CodexErr::InvalidRequest(
                    "thread is live under a different owner".into(),
                ));
            }
            return Ok(NewThread {
                thread_id: id,
                session_configured: thread
                    .startup_metadata()
                    .to_session_configured_event(initial_history.get_event_msgs()),
                thread,
            });
        }
        let registration = if controlled_child {
            control
                .reserve_controlled_resume_registration(
                    &config,
                    thread_id.ok_or_else(|| CodexErr::Fatal("missing resume identity".into()))?,
                    &source,
                )
                .await?
        } else {
            None
        };
        let mut options = StartThreadOptions {
            initial_history,
            session_source: Some(source.clone()),
            thread_source,
            parent_trace,
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        if controlled_child {
            options
                .thread_extension_init
                .insert(crate::session::AgentSessionOwnershipOverride {
                    session_id: control.session_id(),
                });
        }
        let mut request = ThreadSpawnRequest::new(options, auth_manager, control.clone());
        request.registration = ThreadRegistration::Deferred;
        request.parent_thread_id = source.parent_thread_id();
        let resumed = Box::pin(self.state.spawn_thread(request)).await?;
        if resumed.runtime_origin == ThreadRuntimeOrigin::Existing {
            return Err(CodexErr::Fatal(
                "deferred resume adopted an independently loaded runtime".into(),
            ));
        }
        let setup = async {
            let messaging_publication = match live_revert_messaging {
                Some(messaging) => Some(
                    messaging
                        .restore_before_publication(&resumed.thread)
                        .await?,
                ),
                None => {
                    control
                        .restore_agent_send_settings(resumed.thread.session.presentation_id())
                        .await?;
                    None
                }
            };
            if controlled_child {
                control
                    .publish_restored_agent(&resumed.thread, parent.as_ref(), || {
                        if let Some(registration) = registration {
                            registration.commit()?;
                        }
                        Ok(())
                    })
                    .await?;
            } else {
                self.state
                    .publish_restored_thread(&resumed.thread, parent.as_ref(), || Ok(()))
                    .await?;
            }
            if let Some(messaging) = messaging_publication {
                messaging.published();
            }
            Ok::<(), CodexErr>(())
        }
        .await;
        if let Err(error) = setup {
            return Err(control
                .cleanup_unpublished_restoration(&resumed.thread, error)
                .await);
        }
        if !controlled_child {
            self.state.notify_thread_created(resumed.thread_id);
        }
        Ok(resumed.into_new_thread())
    }
}

impl ThreadManagerState {
    pub(crate) async fn reserve_thread_writers(
        &self,
        thread_ids: Vec<ThreadId>,
    ) -> CodexResult<codex_thread_store::ThreadWriterReservation> {
        self.thread_store
            .reserve_thread_writers(thread_ids)
            .await
            .map_err(thread_store_rollout_read_error)
    }

    pub(crate) async fn load_agent_model_context(
        &self,
        thread_id: ThreadId,
        history_mode: ThreadHistoryMode,
    ) -> CodexResult<Option<Vec<RolloutItem>>> {
        match history_mode {
            ThreadHistoryMode::Legacy => Ok(self
                .read_stored_thread(ReadThreadParams {
                    thread_id,
                    include_archived: true,
                    include_history: true,
                })
                .await?
                .history
                .map(|history| history.items)),
            ThreadHistoryMode::Paginated => Ok(Some(
                self.load_latest_model_context(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: true,
                })
                .await?
                .items,
            )),
        }
    }

    pub(super) async fn current_agent_alias(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Option<AgentAlias>> {
        let Some(graph) = self
            .agent_graph_store()
            .filter(|graph| graph.supports_agent_aliases())
        else {
            return Ok(None);
        };
        graph
            .find_current_agent_alias_by_thread(thread_id)
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to resolve current owner for {thread_id}: {error}"
                ))
            })
    }
}
