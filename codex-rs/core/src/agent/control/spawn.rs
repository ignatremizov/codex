use super::residency::is_v2_resident_session_source;
use super::*;
use crate::agent::role::apply_role_to_config;
use crate::codex_thread::CodexThread;
use crate::config::PermissionProfileSnapshot;
use crate::context::ContextualUserFragment;
use crate::context::CurrentTimeReminder;
use crate::context::DeveloperInstructions;
use crate::context::ManagedDeveloperInstructions;
use crate::context::MultiAgentModeInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::SubagentCommentary;
use crate::context::SubagentNotification;
use crate::context::world_state::PersistentModeState;
use crate::session::multi_agents::resolve_usage_hints;
use crate::tools::handlers::multi_agents_common::build_agent_resume_config;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_extension_api::ExtensionDataInit;
use codex_history::rollout::rollout_without_exact_rollback_ranges;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::intersect_effective_permission_profiles;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_utils_path_uri::PathUri;

const AGENT_NAMES: &str = include_str!("../../../assets/agent/agent_names.txt");

struct SpawnAgentThreadInheritance {
    environments: Option<TurnEnvironmentSnapshot>,
    exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
}

/// Initial input delivered after a spawned agent acquires execution capacity.
///
/// V2 communication spawns keep the communication and its context paired so centralized
/// submission and lifecycle logging cannot receive one without the other. Other spawn sources
/// provide user input directly, making an uncontextualized inter-agent communication
/// unrepresentable.
#[allow(clippy::large_enum_variant)]
enum SpawnInitialInput {
    UserInput(Vec<UserInput>),
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext),
}

fn default_agent_nickname_list() -> Vec<&'static str> {
    AGENT_NAMES
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

pub(super) fn agent_nickname_candidates(config: &Config, role_name: Option<&str>) -> Vec<String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    if let Some(candidates) =
        resolve_role_config(config, role_name).and_then(|role| role.nickname_candidates.clone())
    {
        return candidates;
    }

    default_agent_nickname_list()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

fn keep_forked_rollout_item(item: &RolloutItem, preserve_reference_context_item: bool) -> bool {
    match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::Message { role, phase, .. } => match role.as_str() {
                "system" | "developer" | "user" => true,
                "assistant" => *phase == Some(MessagePhase::FinalAnswer),
                _ => false,
            },
            ResponseItem::FunctionCallOutput { call_id: None, .. } => true,
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput {
                call_id: Some(_), ..
            }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => false,
        },
        RolloutItem::RealtimeItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::SecurityRiskScore(_) => false,
        // Full-history forks preserve the cached prompt prefix and can keep diffing
        // from the parent's durable baseline. Truncated forks drop part of that prompt,
        // so they must rebuild context on their first child turn.
        RolloutItem::TurnContext(_) | RolloutItem::WorldState(_) => preserve_reference_context_item,
        // Child threads inherit model context, not the parent's cumulative usage state.
        RolloutItem::TokenUsageRecord(_) => false,
        RolloutItem::Compacted(_) | RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => true,
    }
}

fn retain_forked_developer_message(item: &mut ResponseItem, usage_hint_texts: &[String]) -> bool {
    let ResponseItem::Message { role, .. } = item else {
        return true;
    };
    let role = role.clone();

    let Some(mut content) = to_annotated_content(item) else {
        return false;
    };
    content.retain(|content_item| {
        let ContentItem::InputText { text } = content_item.content() else {
            return true;
        };
        let text_trimmed = text.trim_start();
        if text_trimmed.starts_with("<codex_internal_context source=\"goal\">")
            || text_trimmed.starts_with("<goal_context>")
            || SubagentNotification::matches_text(text)
            || SubagentCommentary::matches_text(text)
        {
            return false;
        }
        if role != "developer" {
            return true;
        }

        !(MultiAgentRoleInstructions::matches_text(text)
            || MultiAgentModeInstructions::matches_text(text)
            || CurrentTimeReminder::matches_text(text)
            || usage_hint_texts
                .iter()
                .any(|usage_hint_text| usage_hint_text == text))
    });
    !content.is_empty() && set_annotated_content(item, content).is_some()
}

async fn load_agent_model_context(
    state: &ThreadManagerState,
    thread_id: ThreadId,
    history_mode: ThreadHistoryMode,
) -> CodexResult<Option<Vec<RolloutItem>>> {
    match history_mode {
        ThreadHistoryMode::Legacy => Ok(state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?
            .history
            .map(|history| history.items)),
        ThreadHistoryMode::Paginated => Ok(Some(
            state
                .load_latest_model_context(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: true,
                })
                .await?
                .items,
        )),
    }
}

async fn apply_restored_v2_agent_role(
    config: &mut Config,
    session_source: &SessionSource,
) -> CodexResult<()> {
    let Some(role_name) = session_source.get_agent_role() else {
        return Ok(());
    };
    let runtime_approval_policy = config.permissions.approval_policy.value();
    let runtime_approvals_reviewer = config.approvals_reviewer;
    let runtime_cwd = config.cwd.clone();
    let runtime_workspace_roots = config.workspace_roots.clone();
    let runtime_workspace_roots_explicit = config.workspace_roots_explicit;
    let runtime_permission_profile = match config.permissions.active_permission_profile() {
        Some(active_permission_profile) => {
            PermissionProfileSnapshot::active_with_profile_workspace_roots(
                config.permissions.permission_profile().clone(),
                active_permission_profile,
                config.permissions.profile_workspace_roots().to_vec(),
            )
        }
        None => PermissionProfileSnapshot::legacy(config.permissions.permission_profile().clone()),
    };

    apply_role_to_config_for_multi_agent_v2(config, Some(&role_name))
        .await
        .map_err(CodexErr::InvalidRequest)?;
    config
        .permissions
        .approval_policy
        .set(runtime_approval_policy)
        .map_err(|err| CodexErr::InvalidRequest(format!("approval_policy is invalid: {err}")))?;
    config.approvals_reviewer = runtime_approvals_reviewer;
    config.cwd = runtime_cwd;
    config.workspace_roots = runtime_workspace_roots;
    config.workspace_roots_explicit = runtime_workspace_roots_explicit;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(runtime_permission_profile)
        .map_err(|err| CodexErr::InvalidRequest(format!("permission_profile is invalid: {err}")))?;
    Ok(())
}

fn apply_restored_agent_model(
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

fn canonical_agent_path(session_source: &SessionSource) -> Option<AgentPath> {
    session_source
        .get_agent_path()
        .or_else(|| (!session_source.is_non_root_agent()).then(AgentPath::root))
}

impl AgentControl {
    fn validate_loaded_v2_agent(
        &self,
        thread: &Arc<CodexThread>,
        expected_session_source: Option<&SessionSource>,
    ) -> CodexResult<()> {
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
        if thread.multi_agent_version() != Some(MultiAgentVersion::V2)
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

    fn validate_loaded_rollout_path(
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

    /// Spawn a new agent thread and submit the initial prompt.
    #[cfg(test)]
    pub(crate) async fn spawn_agent(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
    ) -> CodexResult<ThreadId> {
        let spawned_agent = Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            SpawnAgentOptions::default(),
        ))
        .await?;
        Ok(spawned_agent.thread_id)
    }

    /// Spawn an agent thread with some metadata.
    pub(crate) async fn spawn_agent_with_metadata(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions, // TODO(jif) drop with new fork.
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn spawn_agent_with_communication(
        &self,
        config: Config,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::InterAgentCommunication(communication, context),
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn ensure_v2_agent_loaded(
        &self,
        config: Config,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let resume_lock = state.v2_spawn_resume_lock(thread_id);
        let _resume_guard = resume_lock.lock_owned().await;
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

    #[cfg(test)]
    pub(crate) async fn ensure_v2_agent_loaded_from_source(
        &self,
        config: Config,
        thread_id: ThreadId,
        canonical_session_source: SessionSource,
    ) -> CodexResult<Arc<CodexThread>> {
        self.ensure_v2_agent_loaded_from_source_and_history(
            config,
            thread_id,
            canonical_session_source,
            /*initial_history_override*/ None,
            /*client_mcp_extensions_override*/ None,
        )
        .await
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
            self.state
                .reserve_agent_metadata_replacement(
                    thread_id,
                    AgentMetadata {
                        agent_id: Some(thread_id),
                        agent_path: canonical_agent_path(&canonical_session_source),
                        agent_nickname: canonical_session_source.get_nickname(),
                        agent_role: canonical_session_source.get_agent_role(),
                        last_task_message,
                    },
                )?
                .commit()?;
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
                Some(canonical_session_source),
                initial_history_override,
                client_mcp_extensions_override,
            )
            .await;
        match load_result {
            Ok(thread) => {
                if let Err(err) = metadata_replacement.commit() {
                    let terminal_presentation_disarm =
                        thread.session.disarm_terminal_presentation();
                    let cleanup_result = self
                        .discard_live_agent_instance(&thread, LiveAgentMetadataDisposition::Release)
                        .await;
                    terminal_presentation_disarm.commit();
                    if let Err(cleanup_error) = cleanup_result {
                        return Err(CodexErr::Fatal(format!(
                            "{err}; failed to discard incompatible restored runtime: {cleanup_error}"
                        )));
                    }
                    return Err(err);
                }
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
        let expected_rollout_path = initial_history_override
            .as_ref()
            .and_then(|initial_history| match initial_history {
                InitialHistory::Resumed(resumed) => resumed.rollout_path.clone(),
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
            });
        if let Ok(thread) = state.get_thread(thread_id).await {
            self.validate_loaded_v2_agent(&thread, canonical_session_source.as_ref())?;
            self.validate_loaded_rollout_path(&thread, expected_rollout_path.as_deref())?;
            self.touch_loaded_v2_residency(&state, thread_id).await;
            return Ok(thread);
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
            session_source
                .parent_thread_id()
                .or_else(|| initial_history.get_resumed_parent_thread_id())
                .or(stored_parent_thread_id)
                .or(registered_parent_thread_id)
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
                    let turn = parent.session.new_default_turn().await;
                    config = build_agent_resume_config(&turn).map_err(|_| {
                        CodexErr::InvalidRequest(format!(
                            "cannot resume multi-agent v2 child {thread_id} with the current parent settings"
                        ))
                    })?;
                    Some((parent, turn.environments.clone()))
                }
                Ok(_) => {
                    return Err(CodexErr::InvalidRequest(format!(
                        "cannot resume multi-agent v2 child {thread_id}: parent ownership is unavailable"
                    )));
                }
                Err(_) => None,
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
        config.service_tier = self.root_service_tier();
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
            if let Some(selections) = environment_selections.as_mut() {
                for selection in selections {
                    let environment_id = &selection.environment_id;
                    let invalid_environment = |reason: &str| {
                        CodexErr::InvalidRequest(format!(
                            "cannot resume multi-agent v2 child {thread_id}: cached environment {environment_id} {reason}"
                        ))
                    };
                    // Matching the attachment also keeps startup on the captured owner executor.
                    let owner_environment = parent_environments
                        .turn_environments()
                        .find(|environment| {
                            let parent_selection = &environment.selection;
                            parent_selection.environment_id == selection.environment_id
                                && parent_selection.cwd == selection.cwd
                                && parent_selection.workspace_roots == selection.workspace_roots
                        })
                        .ok_or_else(|| {
                            invalid_environment("no longer matches a ready parent environment")
                        })?;
                    let owner_config = owner_environment.config();
                    let child_config = match &selection.config {
                        EnvironmentConfigState::FromThread => {
                            // Pin current owner authority instead of re-inferring child settings.
                            selection.config = EnvironmentConfigState::Ready(owner_config.clone());
                            continue;
                        }
                        EnvironmentConfigState::Ready(config) => config,
                        EnvironmentConfigState::Pending | EnvironmentConfigState::Failed(_) => {
                            return Err(invalid_environment("configuration is not ready"));
                        }
                    };
                    let mut bounded_config = child_config.clone();
                    bounded_config.permission_profile = owner_config.permission_profile.clone();
                    if bounded_config != *owner_config {
                        return Err(invalid_environment(
                            "configuration differs from the current parent",
                        ));
                    }
                    if child_config.permission_profile == owner_config.permission_profile {
                        continue;
                    }
                    if owner_environment.environment.is_remote() {
                        return Err(invalid_environment(
                            "permissions changed on a remote executor",
                        ));
                    }
                    let cwd = selection.cwd.to_abs_path().map_err(|_| {
                        invalid_environment("working directory is not a local absolute path")
                    })?;
                    let roots = owner_environment
                        .workspace_roots()
                        .iter()
                        .map(PathUri::to_abs_path)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            invalid_environment("workspace roots are not local absolute paths")
                        })?;
                    let authority = owner_environment
                        .permission_profile()
                        .clone()
                        .materialize_project_roots_with_workspace_roots(&roots);
                    let requested = child_config
                        .permission_profile
                        .permission_profile()
                        .clone()
                        .materialize_project_roots_with_workspace_roots(&roots);
                    let permissions =
                        intersect_effective_permission_profiles(&authority, &requested, &cwd)
                            .map_err(|err| {
                                invalid_environment(&format!(
                                    "permissions cannot be intersected safely: {err}"
                                ))
                            })?;
                    bounded_config.permission_profile =
                        PermissionProfileSnapshot::legacy(permissions);
                    selection.config = EnvironmentConfigState::Ready(bounded_config);
                }
            }
            (
                Some(parent_environments.clone()),
                Some(Arc::clone(&parent.session.services.exec_policy)),
                client_mcp_extensions_override
                    .or_else(|| Some(parent.client_mcp_extensions())),
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
        // Reserving a slot can evict an idle nested parent. Keep its authority captured above.
        let residency_slot = self
            .reserve_v2_residency_slot(&state, &config, Some(thread_id))
            .await?;
        let notification_source = session_source.clone();
        match state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config,
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections,
                inherited_environments,
                inherited_exec_policy,
                client_mcp_extensions_override,
            })
            .await
        {
            Ok(reloaded_thread) => {
                self.validate_loaded_v2_agent(&reloaded_thread.thread, Some(&notification_source))?;
                self.validate_loaded_rollout_path(
                    &reloaded_thread.thread,
                    expected_rollout_path.as_deref(),
                )?;
                self.state.clear_evicted_environments(thread_id);
                residency_slot.commit(reloaded_thread.thread_id);
                state.notify_thread_created(reloaded_thread.thread_id);
                let child_agent_path = notification_source.get_agent_path();
                let child_reference = child_agent_path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| reloaded_thread.thread_id.to_string());
                self.maybe_start_completion_watcher(
                    &reloaded_thread.thread,
                    Some(notification_source),
                    child_reference,
                    child_agent_path,
                    MultiAgentVersion::V2,
                )
                .await;
                Ok(reloaded_thread.thread)
            }
            Err(err) => {
                if let Ok(thread) = state.get_thread(thread_id).await {
                    self.validate_loaded_v2_agent(&thread, Some(&notification_source))?;
                    self.validate_loaded_rollout_path(&thread, expected_rollout_path.as_deref())?;
                    self.state.clear_evicted_environments(thread_id);
                    drop(residency_slot);
                    self.touch_loaded_v2_residency(&state, thread_id).await;
                    return Ok(thread);
                }
                Err(err)
            }
        }
    }

    async fn spawn_agent_internal(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        let state = self.upgrade()?;
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &InitialHistory::New,
                session_source.as_ref(),
                options.parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        if let Some(session_source) = session_source.as_ref() {
            self.ensure_execution_capacity(multi_agent_version, session_source)?;
        }
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let spawn_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && session_source
                .as_ref()
                .is_some_and(is_v2_resident_session_source);
        let residency_slot = if spawn_uses_v2_residency {
            Some(
                self.reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
                    .await?,
            )
        } else {
            None
        };
        let reservation_max_threads = if spawn_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = self.state.reserve_spawn_slot(reservation_max_threads)?;
        let inheritance = SpawnAgentThreadInheritance {
            environments: self
                .inherited_environments_for_source(&state, session_source.as_ref())
                .await,
            exec_policy: self
                .inherited_exec_policy_for_source(&state, session_source.as_ref(), &config)
                .await,
        };
        let (session_source, mut agent_metadata) = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                ..
            })) => {
                let (session_source, agent_metadata) = self.prepare_thread_spawn(
                    &mut reservation,
                    &config,
                    parent_thread_id,
                    depth,
                    agent_path,
                    agent_role,
                    /*preferred_agent_nickname*/ None,
                )?;
                (Some(session_source), agent_metadata)
            }
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();

        // The same `AgentControl` is sent to spawn the thread.
        let new_thread = match (session_source, options.fork_mode.as_ref(), inheritance) {
            (Some(session_source), Some(_), inheritance) => {
                Box::pin(self.spawn_forked_thread(
                    &state,
                    config,
                    session_source,
                    &options,
                    inheritance,
                    multi_agent_version,
                ))
                .await?
            }
            (Some(session_source), None, inheritance) => {
                let history_mode = if let Some(parent_thread_id) = options.parent_thread_id
                    && let Ok(parent_thread) = state.get_thread(parent_thread_id).await
                {
                    matches!(
                        parent_thread.config_snapshot().await.history_mode,
                        ThreadHistoryMode::Paginated
                    )
                    .then_some(ThreadHistoryMode::Paginated)
                } else {
                    None
                };
                Box::pin(state.spawn_new_thread_with_source(
                    config.clone(),
                    self.clone(),
                    session_source,
                    history_mode,
                    options.parent_thread_id,
                    /*forked_from_thread_id*/ None,
                    /*thread_source*/ Some(ThreadSource::Subagent),
                    /*metrics_service_name*/ None,
                    inheritance.environments,
                    inheritance.exec_policy,
                    options.environments.clone(),
                ))
                .await?
            }
            (None, _, _) => Box::pin(state.spawn_new_thread(config.clone(), self.clone())).await?,
        };
        agent_metadata.agent_id = Some(new_thread.thread_id);
        reservation.commit(agent_metadata.clone());
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(new_thread.thread_id);
        }

        if let Some(SessionSource::SubAgent(
            subagent_source @ SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            },
        )) = notification_source.as_ref()
        {
            let client_metadata = match state.get_thread(*parent_thread_id).await {
                Ok(parent_thread) => parent_thread.session.app_server_client_metadata().await,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        parent_thread_id = %parent_thread_id,
                        "skipping subagent thread analytics: failed to load parent thread metadata"
                    );
                    crate::session::session::AppServerClientMetadata {
                        client_name: None,
                        client_version: None,
                    }
                }
            };
            let thread_config = new_thread.thread.config_snapshot().await;
            let parent_thread_id = thread_config.parent_thread_id;
            emit_subagent_session_started(
                &new_thread.thread.session.services.analytics_events_client,
                client_metadata,
                new_thread.thread.session.session_id(),
                new_thread.thread_id,
                parent_thread_id,
                thread_config,
                subagent_source.clone(),
            );
        }

        let child_reference = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| new_thread.thread_id.to_string());
        // Attach before exposing the child or submitting its first input so an early failure
        // cannot publish a watcher-owned terminal without a consumer.
        self.maybe_start_completion_watcher(
            &new_thread.thread,
            notification_source.clone(),
            child_reference,
            agent_metadata.agent_path.clone(),
            new_thread
                .thread
                .multi_agent_version()
                .unwrap_or(MultiAgentVersion::V1),
        )
        .await;

        if matches!(
            notification_source.as_ref(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
        ) {
            new_thread.thread.ensure_rollout_materialized().await;
        }

        // Notify a new thread has been created. This notification will be processed by clients
        // to subscribe or drain this newly created thread.
        // TODO(jif) add helper for drain
        state.notify_thread_created(new_thread.thread_id);

        self.persist_thread_spawn_edge_for_source(
            new_thread.thread.as_ref(),
            new_thread.thread_id,
            notification_source.as_ref(),
        )
        .await;

        let start_options = TurnStartOptions {
            parent_turn_id: options.parent_turn_id,
            root_turn_id: options.root_turn_id,
            cyber_access_program: options.cyber_access_program,
            ..Default::default()
        };
        match initial_input {
            SpawnInitialInput::UserInput(input) => {
                self.send_input(new_thread.thread_id, input, start_options)
                    .await?;
            }
            SpawnInitialInput::InterAgentCommunication(communication, context) => {
                self.send_inter_agent_communication_after_capacity_check(
                    new_thread.thread_id,
                    &state,
                    communication,
                    context,
                    start_options,
                )
                .await?;
            }
        }

        Ok(LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status: self.get_status(new_thread.thread_id).await,
        })
    }

    async fn spawn_forked_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        config: Config,
        session_source: SessionSource,
        options: &SpawnAgentOptions,
        inheritance: SpawnAgentThreadInheritance,
        multi_agent_version: MultiAgentVersion,
    ) -> CodexResult<crate::thread_manager::NewThread> {
        let SpawnAgentThreadInheritance {
            environments: inherited_environments,
            exec_policy: inherited_exec_policy,
        } = inheritance;
        if options.fork_parent_spawn_call_id.is_none() {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a parent spawn call id".to_string(),
            ));
        }
        let Some(fork_mode) = options.fork_mode.as_ref() else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a fork mode".to_string(),
            ));
        };
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &session_source
        else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a thread-spawn session source".to_string(),
            ));
        };

        let parent_thread_id = *parent_thread_id;
        let parent_thread = state.get_thread(parent_thread_id).await?;
        let (subagent_developer_instructions, parent_developer_instructions) = match (
            multi_agent_version,
            config
                .multi_agent_v2
                .subagent_developer_instructions
                .as_ref(),
        ) {
            (MultiAgentVersion::V2, override_instructions)
                if override_instructions.is_some() || session_source.get_agent_role().is_some() =>
            {
                let parent_developer_instructions = match parent_thread
                    .session
                    .new_default_turn()
                    .await
                    .developer_instructions
                    .clone()
                {
                    Some(instructions) if !instructions.is_empty() => Some(instructions),
                    Some(_) | None => None,
                };
                (
                    Some(config.developer_instructions.clone().unwrap_or_default()),
                    parent_developer_instructions,
                )
            }
            (MultiAgentVersion::Disabled | MultiAgentVersion::V1, _)
            | (MultiAgentVersion::V2, _) => (None, None),
        };
        let parent_history_mode = parent_thread.config_snapshot().await.history_mode;
        // `record_conversation_items` only queues persistence writes asynchronously.
        // Flush before snapshotting store history for a fork.
        parent_thread.ensure_rollout_materialized().await;
        parent_thread.flush_rollout().await?;

        let destination_history_mode = matches!(parent_history_mode, ThreadHistoryMode::Paginated)
            .then_some(ThreadHistoryMode::Paginated);
        let forked_rollout_items =
            load_agent_model_context(state, parent_thread_id, parent_history_mode)
                .await?
                .ok_or_else(|| {
                    CodexErr::Fatal(format!(
                        "parent thread history unavailable for fork: {parent_thread_id}"
                    ))
                })?;
        let mut forked_rollout_items = rollout_without_exact_rollback_ranges(&forked_rollout_items);

        let selected_capability_roots = forked_rollout_items
            .iter()
            .find_map(|item| {
                let RolloutItem::SessionMeta(meta_line) = item else {
                    return None;
                };
                Some(meta_line.meta.selected_capability_roots.clone())
            })
            .unwrap_or_default();
        if let SpawnAgentForkMode::LastNTurns(last_n_turns) = fork_mode {
            forked_rollout_items =
                truncate_rollout_to_last_n_fork_turns(forked_rollout_items, *last_n_turns);
        }
        let multi_agent_v2_usage_hint_texts_to_filter: Vec<String> =
            if multi_agent_version == MultiAgentVersion::V2 {
                let parent_config = parent_thread.session.get_config().await;
                let parent_usage_hints = resolve_usage_hints(
                    &parent_config.multi_agent_v2,
                    /*catalog*/ None,
                    !parent_config.update_plan_enabled,
                );
                [parent_usage_hints.root, parent_usage_hints.subagent]
                    .into_iter()
                    .flatten()
                    .map(|instructions| instructions.render())
                    .collect()
            } else {
                Vec::new()
            };
        let mut preserve_reference_context_item =
            matches!(fork_mode, SpawnAgentForkMode::FullHistory);
        if preserve_reference_context_item {
            for item in forked_rollout_items.iter().rev() {
                let RolloutItem::Compacted(compacted) = item else {
                    continue;
                };
                // Legacy checkpoints force the child to rebuild context regardless of the
                // live parent's reference baseline; an older superseded checkpoint does not.
                if compacted.replacement_history.is_none() {
                    preserve_reference_context_item = false;
                }
                break;
            }
        }
        let mut replaced_parent_developer_instructions = false;
        // Scrub inherited hints and replace only the parent's developer-instruction fragment.
        // Compaction stores response items separately, so sanitize both top-level messages and
        // compacted replacement histories with the same policy.
        let retain_forked_item = |response_item: &mut ResponseItem, replaced: &mut bool| {
            if matches!(response_item, ResponseItem::AgentMessage { .. }) {
                return false;
            }
            if !retain_forked_developer_message(
                response_item,
                &multi_agent_v2_usage_hint_texts_to_filter,
            ) {
                return false;
            }

            if matches!(response_item, ResponseItem::Message { role, .. } if role == "developer") {
                let Some(mut content) = to_annotated_content(response_item) else {
                    return false;
                };
                content.retain_mut(|content_item| {
                    let ContentItem::InputText { text } = content_item.content_mut() else {
                        return true;
                    };
                    if ManagedDeveloperInstructions::matches_text(text)
                        || PersistentModeState::matches_text(text)
                    {
                        // If the child will rebuild its initial context, drop the inherited
                        // instructions; startup will add the current requirements and effort
                        // instructions once.
                        return preserve_reference_context_item;
                    }
                    let (
                        Some(parent_developer_instructions),
                        Some(subagent_developer_instructions),
                    ) = (
                        parent_developer_instructions.as_ref(),
                        subagent_developer_instructions.as_ref(),
                    )
                    else {
                        return true;
                    };
                    // TODO(anp) track better message fragment provenance in rollouts.
                    if !text.contains(parent_developer_instructions) {
                        return true;
                    }

                    *replaced = true;
                    let replacement = if preserve_reference_context_item {
                        subagent_developer_instructions.as_str()
                    } else {
                        ""
                    };
                    *text = text.replace(parent_developer_instructions, replacement);
                    !text.is_empty()
                });
                return !content.is_empty()
                    && set_annotated_content(response_item, content).is_some();
            }

            true
        };
        forked_rollout_items.retain_mut(|item| {
            if !keep_forked_rollout_item(item, preserve_reference_context_item)
                || destination_history_mode == Some(ThreadHistoryMode::Paginated)
                    && matches!(
                        &*item,
                        RolloutItem::EventMsg(
                            EventMsg::ItemCompleted(_)
                                | EventMsg::TokenCount(_)
                                | EventMsg::ThreadGoalUpdated(_)
                                | EventMsg::ThreadSettingsApplied(_),
                        )
                    )
            {
                return false;
            }

            match item {
                RolloutItem::ResponseItem(response_item) => {
                    retain_forked_item(response_item, &mut replaced_parent_developer_instructions)
                }
                RolloutItem::Compacted(compacted) => {
                    // This checkpoint belongs to the inherited parent prefix.
                    compacted.latest_token_usage_record = None;
                    // Parent-local review evidence must not become the child's authorization.
                    // Root user authorization is collected separately by the host.
                    compacted.guardian_history = None;
                    if compacted.replacement_history.is_some() {
                        // Matches before this checkpoint cannot survive its replacement history.
                        replaced_parent_developer_instructions = false;
                        compacted.retain_replacement_history_items(|response_item| {
                            retain_forked_item(
                                response_item,
                                &mut replaced_parent_developer_instructions,
                            )
                        });
                    }
                    true
                }
                RolloutItem::WorldState(world_state) => {
                    if multi_agent_version == MultiAgentVersion::V2 {
                        world_state.state.remove("multi_agent_usage_hint");
                    }
                    true
                }
                RolloutItem::RealtimeItem(_) => false,
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => true,
                RolloutItem::TokenUsageRecord(_) | RolloutItem::SecurityRiskScore(_) => false,
            }
        });
        // Full forks reuse the parent's reference context instead of rebuilding it. If that
        // context omitted the parent's developer fragment, append the child's override so its
        // instructions still reach the model exactly once.
        if let Some(subagent_developer_instructions) = subagent_developer_instructions.as_ref()
            && preserve_reference_context_item
            && !replaced_parent_developer_instructions
            && !subagent_developer_instructions.is_empty()
            && parent_thread
                .session
                .reference_context_item()
                .await
                .is_some()
        {
            let developer_message = ContextualUserFragment::into(DeveloperInstructions::new(
                subagent_developer_instructions,
            ));
            forked_rollout_items.push(RolloutItem::ResponseItem(developer_message.into()));
        }
        if preserve_reference_context_item
            && multi_agent_version == MultiAgentVersion::V2
            && let Some(subagent_usage_hint) = options
                .multi_agent_v2_usage_hints
                .as_ref()
                .map(|hints| hints.subagent.clone())
                .unwrap_or_else(|| {
                    resolve_usage_hints(
                        &config.multi_agent_v2,
                        /*catalog*/ None,
                        !config.update_plan_enabled,
                    )
                    .subagent
                })
        {
            let subagent_usage_hint_message = ContextualUserFragment::into(subagent_usage_hint);
            forked_rollout_items.push(RolloutItem::ResponseItem(
                subagent_usage_hint_message.into(),
            ));
        }
        let mut thread_extension_init = ExtensionDataInit::new();
        thread_extension_init.insert(selected_capability_roots);

        state
            .fork_thread_with_source(
                config.clone(),
                InitialHistory::Forked(forked_rollout_items),
                destination_history_mode,
                self.clone(),
                session_source,
                /*thread_source*/ Some(ThreadSource::Subagent),
                /*parent_thread_id*/ Some(parent_thread_id),
                /*forked_from_thread_id*/ Some(parent_thread_id),
                inherited_environments,
                inherited_exec_policy,
                options.environments.clone(),
                thread_extension_init,
            )
            .await
    }

    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let root_depth = thread_spawn_depth(&session_source).unwrap_or(0);
        let (resumed_thread, resumed_multi_agent_version) =
            Box::pin(self.resume_single_agent_from_rollout(
                config.clone(),
                thread_id,
                session_source,
                /*initial_history_override*/ None,
                /*client_mcp_extensions_override*/ None,
            ))
            .await?;
        let resumed_thread_id = resumed_thread.session.thread_id();
        let state = self.upgrade()?;
        if config.multi_agent_version_from_features() == MultiAgentVersion::V2
            || resumed_multi_agent_version == MultiAgentVersion::V2
        {
            return Ok(resumed_thread_id);
        }
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(resumed_thread_id);
        };

        let mut resume_queue = VecDeque::from([(thread_id, root_depth)]);
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
                let child_depth = parent_depth + 1;
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
                        Ok((_, _)) => true,
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

        Ok(resumed_thread_id)
    }

    pub(crate) async fn resume_v2_agent_from_history(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        initial_history: InitialHistory,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<Arc<CodexThread>> {
        let (resumed_thread, multi_agent_version) =
            Box::pin(self.resume_single_agent_from_rollout(
                config,
                thread_id,
                session_source,
                Some(initial_history),
                Some(client_mcp_extensions),
            ))
            .await?;
        if multi_agent_version != MultiAgentVersion::V2 {
            return Err(CodexErr::InvalidRequest(format!(
                "persisted spawned child {thread_id} is not running Multi-Agent V2"
            )));
        }
        Ok(resumed_thread)
    }

    async fn resume_single_agent_from_rollout(
        &self,
        mut config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        initial_history_override: Option<InitialHistory>,
        client_mcp_extensions_override: Option<ClientMcpExtensions>,
    ) -> CodexResult<(Arc<CodexThread>, MultiAgentVersion)> {
        let state = self.upgrade()?;
        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let stored_model = stored_thread.model.clone();
        let stored_model_provider = stored_thread.model_provider.clone();
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
                if let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
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
        if multi_agent_version == MultiAgentVersion::V2 {
            apply_restored_agent_model(&mut config, stored_model, stored_model_provider)?;
        }
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
        let inherited_environments = self
            .inherited_environments_for_source(&state, Some(&session_source))
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, Some(&session_source), &config)
            .await;

        let resumed_thread = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config: config.clone(),
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections: None,
                inherited_environments,
                inherited_exec_policy,
                client_mcp_extensions_override,
            })
            .await?;
        if multi_agent_version == MultiAgentVersion::V2 {
            self.validate_loaded_v2_agent(&resumed_thread.thread, Some(&notification_source))?;
            self.validate_loaded_rollout_path(
                &resumed_thread.thread,
                expected_rollout_path.as_deref(),
            )?;
        }
        let mut agent_metadata = agent_metadata;
        agent_metadata.agent_id = Some(resumed_thread.thread_id);
        if register_resumed_agent {
            reservation.commit_if_absent(agent_metadata.clone());
        }
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(resumed_thread.thread_id);
        }
        // Resumed threads are re-registered in-memory and need the same listener
        // attachment path as freshly spawned threads.
        state.notify_thread_created(resumed_thread.thread_id);
        let child_reference = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| resumed_thread.thread_id.to_string());
        self.maybe_start_completion_watcher(
            &resumed_thread.thread,
            Some(notification_source.clone()),
            child_reference,
            agent_metadata.agent_path.clone(),
            resumed_thread
                .thread
                .multi_agent_version()
                .unwrap_or(MultiAgentVersion::V1),
        )
        .await;
        self.persist_thread_spawn_edge_for_source(
            resumed_thread.thread.as_ref(),
            resumed_thread.thread_id,
            Some(&notification_source),
        )
        .await;

        Ok((resumed_thread.thread, multi_agent_version))
    }
}
