use super::residency::is_v2_resident_session_source;
use super::setup_cleanup::SetupCleanupGuard;
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
use crate::context::UserAgentTask;
use crate::thread_manager::ThreadRuntimePublication;
use codex_extension_api::ExtensionDataInit;
use codex_history::rollout::rollout_without_exact_rollback_ranges;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::intersect_effective_permission_profiles;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_utils_path_uri::PathUri;
use std::collections::HashSet;

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
    None,
    UserInput {
        input: Vec<UserInput>,
        user_task_preview: Option<String>,
        post_admission_failure: SpawnPostAdmissionFailure,
    },
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext),
}

#[derive(Clone, Copy)]
enum SpawnPostAdmissionFailure {
    Strict,
    PreserveAdmittedUserWork,
}

struct ResumeSingleAgentOptions {
    config: Config,
    thread_id: ThreadId,
    session_source: SessionSource,
    response_observer_source: SessionSource,
    initial_history_override: Option<InitialHistory>,
    client_mcp_extensions_override: Option<ClientMcpExtensions>,
    response_observation: ResponseObservationPolicy,
    response_observer: ResponseObserverKind,
    initial_terminal_observation: InitialTerminalObservation,
    thread_spawn_persistence: ThreadSpawnPersistence,
}

struct ResumeAgentControlOptions {
    response_observation: ResponseObservationPolicy,
    response_observer: ResponseObserverKind,
    initial_terminal_observation: InitialTerminalObservation,
    durable_response_observer_source: Option<SessionSource>,
    initial_user_input: Option<ResumeUserInputAdmission>,
    thread_spawn_persistence: ThreadSpawnPersistence,
}

struct DeferredResumeResponseObserver {
    source: SessionSource,
    response_observation: ResponseObservationPolicy,
    response_observer: ResponseObserverKind,
    initial_terminal_observation: InitialTerminalObservation,
}

struct ResumeAgentControlOutcome {
    thread_id: ThreadId,
    initial_submission: Option<ResponseObservationSubmission>,
    persisted_alias: Option<AgentAlias>,
}

struct ResumeSingleAgentOutcome {
    thread: Arc<CodexThread>,
    multi_agent_version: MultiAgentVersion,
    runtime_origin: crate::thread_manager::ThreadRuntimeOrigin,
    persisted_alias: Option<AgentAlias>,
    setup_cleanup: Option<SetupCleanupGuard>,
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
        return candidates
            .into_iter()
            .filter(|candidate| !candidate.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME))
            .collect();
    }

    default_agent_nickname_list()
        .into_iter()
        .filter(|candidate| !candidate.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME))
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
        | RolloutItem::AgentResponseObservation(_)
        | RolloutItem::SecurityRiskScore(_) => false,
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
            if matches!(&event.item, TurnItem::UserAgentControl(_)) =>
        {
            // User-agent control items describe actions authored from one source transcript.
            // Copying them into a child would falsely attribute those source-only actions to the
            // fork, while the canonical audit remains durable in the original rollout.
            false
        }
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
            || UserAgentTask::matches_text(text)
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
                let canonical_source = state
                    .load_agent_model_context(thread_id, stored_thread.history_mode)
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
            SpawnInitialInput::UserInput {
                input: initial_input,
                user_task_preview: None,
                post_admission_failure: SpawnPostAdmissionFailure::Strict,
            },
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
            SpawnInitialInput::UserInput {
                input: initial_input,
                user_task_preview: None,
                post_admission_failure: SpawnPostAdmissionFailure::Strict,
            },
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn spawn_user_agent_with_metadata(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        user_task_preview: Option<String>,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput {
                input: initial_input,
                user_task_preview,
                post_admission_failure: SpawnPostAdmissionFailure::PreserveAdmittedUserWork,
            },
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn spawn_idle_agent_with_metadata(
        &self,
        config: Config,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::None,
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
        let parent_thread_id = match state.get_thread(thread_id).await {
            Ok(thread) => thread.session_source.parent_thread_id(),
            Err(_) => state
                .read_stored_thread(ReadThreadParams {
                    thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await?
                .source
                .parent_thread_id(),
        };
        let _parent_lifecycle_guard = if let Some(parent_thread_id) = parent_thread_id {
            Some(state.acquire_live_agent_lifecycle(parent_thread_id).await?)
        } else {
            None
        };
        let resume_lock = state.agent_lifecycle_lock(thread_id);
        let _resume_guard = resume_lock.lock_owned().await;
        self.require_current_agent_ownership(thread_id).await?;
        if let Ok(thread) = state.get_thread(thread_id).await {
            return self
                .ensure_v2_agent_loaded_from_source_and_history(
                    config,
                    thread_id,
                    thread.session_source.clone(),
                    /*initial_history_override*/ None,
                    /*client_mcp_extensions_override*/ None,
                    ThreadCreatedPublication::Immediate,
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
        let history = state
            .load_agent_model_context(thread_id, stored_thread.history_mode)
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
            ThreadCreatedPublication::Immediate,
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
            ThreadCreatedPublication::Immediate,
        )
        .await
        .map(|loaded| loaded.thread)
    }

    /// Reload a V2 child while the caller holds its direct parent's lifecycle guard.
    pub(crate) async fn ensure_v2_agent_loaded_from_history(
        &self,
        config: Config,
        thread_id: ThreadId,
        canonical_session_source: SessionSource,
        initial_history: InitialHistory,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<crate::thread_manager::ThreadSpawnResult> {
        self.ensure_v2_agent_loaded_from_source_and_history(
            config,
            thread_id,
            canonical_session_source,
            Some(initial_history),
            Some(client_mcp_extensions),
            ThreadCreatedPublication::Deferred,
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
        thread_created_publication: ThreadCreatedPublication,
    ) -> CodexResult<crate::thread_manager::ThreadSpawnResult> {
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
            self.restore_agent_metadata(
                thread_id,
                AgentMetadata {
                    agent_id: Some(thread_id),
                    agent_path: canonical_agent_path(&canonical_session_source),
                    agent_nickname: canonical_session_source.get_nickname(),
                    agent_role: canonical_session_source.get_agent_role(),
                    last_task_message,
                },
            )?;
            self.touch_loaded_v2_residency(&state, thread_id).await;
            return Ok(crate::thread_manager::ThreadSpawnResult {
                thread_id,
                session_configured: thread.session_configured(),
                thread,
                runtime_origin: crate::thread_manager::ThreadRuntimeOrigin::Existing,
                setup_cleanup: None,
            });
        }
        let previous_metadata = self.state.agent_metadata_for_thread(thread_id);
        let last_task_message = previous_metadata
            .as_ref()
            .and_then(|metadata| metadata.last_task_message.clone());
        let replacement_metadata = AgentMetadata {
            agent_id: Some(thread_id),
            agent_path: canonical_agent_path(&canonical_session_source),
            agent_nickname: canonical_session_source.get_nickname(),
            agent_role: canonical_session_source.get_agent_role(),
            last_task_message,
        };
        let metadata_replacement = self
            .state
            .reserve_agent_metadata_replacement(thread_id, replacement_metadata.clone())?;
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
            Ok(mut loaded) => {
                if let Err(err) = metadata_replacement.commit() {
                    let cleanup_result = loaded.rollback_setup_cleanup().await;
                    if let Err(cleanup_error) = cleanup_result {
                        return Err(CodexErr::Fatal(format!(
                            "{err}; failed to discard incompatible restored runtime: {cleanup_error}"
                        )));
                    }
                    return Err(err);
                }
                if loaded.runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Created {
                    loaded.disarm_setup_cleanup();
                    loaded.attach_setup_cleanup(SetupCleanupGuard::new_with_agent_lifecycle(
                        "publish reloaded V2 agent",
                        Arc::clone(&state),
                        loaded.thread_id,
                        {
                            let control = self.clone();
                            let state = Arc::clone(&state);
                            let thread = Arc::clone(&loaded.thread);
                            let previous_metadata = previous_metadata.clone();
                            let replacement_metadata = replacement_metadata.clone();
                            async move {
                                let owns_runtime =
                                    state.thread_instance_is_current_or_pending(&thread).await;
                                let metadata_disposition = if previous_metadata.is_some() {
                                    LiveAgentMetadataDisposition::Preserve
                                } else {
                                    LiveAgentMetadataDisposition::Release
                                };
                                let runtime_cleanup = control
                                    .discard_unpublished_agent_instance(
                                        &thread,
                                        metadata_disposition,
                                    )
                                    .await;
                                let metadata_restore = if owns_runtime {
                                    match previous_metadata {
                                        Some(previous_metadata) => control
                                            .restore_agent_metadata_if_current(
                                                thread.session.thread_id(),
                                                &replacement_metadata,
                                                previous_metadata,
                                            )
                                            .map(drop),
                                        None => Ok(()),
                                    }
                                } else {
                                    Ok(())
                                };
                                runtime_cleanup.and(metadata_restore)
                            }
                        },
                    ));
                }
                if thread_created_publication == ThreadCreatedPublication::Immediate
                    && loaded.runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Created
                    && let Err(err) = state.publish_thread(&loaded.thread).await
                {
                    let cleanup_result = loaded.rollback_setup_cleanup().await;
                    if let Err(cleanup_error) = cleanup_result {
                        return Err(CodexErr::Fatal(format!(
                            "{err}; failed to discard unpublished restored runtime: \
                             {cleanup_error}"
                        )));
                    }
                    return Err(err);
                }
                if thread_created_publication == ThreadCreatedPublication::Immediate
                    && loaded.runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Created
                {
                    loaded.disarm_setup_cleanup();
                    state.notify_thread_created(loaded.thread_id);
                }
                Ok(loaded)
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
    ) -> CodexResult<crate::thread_manager::ThreadSpawnResult> {
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
            return Ok(crate::thread_manager::ThreadSpawnResult {
                thread_id,
                session_configured: thread.session_configured(),
                thread,
                runtime_origin: crate::thread_manager::ThreadRuntimeOrigin::Existing,
                setup_cleanup: None,
            });
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
                    let history = state
                        .load_agent_model_context(thread_id, stored_thread.history_mode)
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
                runtime_publication: ThreadRuntimePublication::Deferred,
            })
            .await
        {
            Ok(mut reloaded_thread) => {
                if reloaded_thread.runtime_origin
                    == crate::thread_manager::ThreadRuntimeOrigin::Created
                {
                    reloaded_thread.attach_setup_cleanup(
                        SetupCleanupGuard::new_with_agent_lifecycle(
                            "reload V2 agent",
                            Arc::clone(&state),
                            reloaded_thread.thread_id,
                            {
                                let control = self.clone();
                                let thread = Arc::clone(&reloaded_thread.thread);
                                async move {
                                    control
                                        .discard_unpublished_agent_instance(
                                            &thread,
                                            LiveAgentMetadataDisposition::Preserve,
                                        )
                                        .await
                                }
                            },
                        ),
                    );
                }
                let setup_result: CodexResult<()> = async {
                    self.validate_loaded_v2_agent(
                        &reloaded_thread.thread,
                        Some(&notification_source),
                    )?;
                    self.validate_loaded_rollout_path(
                        &reloaded_thread.thread,
                        expected_rollout_path.as_deref(),
                    )?;
                    residency_slot.commit(reloaded_thread.thread_id);
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
                        ResponseObservationPolicy::default(),
                        ResponseObserverKind::Native,
                        InitialTerminalObservation::FutureTurnsOnly,
                    )
                    .await?;
                    self.state.clear_evicted_environments(thread_id);
                    state.notify_thread_created(reloaded_thread.thread_id);
                    Ok(())
                }
                .await;
                if let Err(err) = setup_result {
                    reloaded_thread.rollback_setup_cleanup().await?;
                    return Err(err);
                }
                Ok(reloaded_thread)
            }
            Err(err) => {
                if let Ok(thread) = state.get_thread(thread_id).await {
                    self.validate_loaded_v2_agent(&thread, Some(&notification_source))?;
                    self.validate_loaded_rollout_path(&thread, expected_rollout_path.as_deref())?;
                    self.state.clear_evicted_environments(thread_id);
                    drop(residency_slot);
                    self.touch_loaded_v2_residency(&state, thread_id).await;
                    return Ok(crate::thread_manager::ThreadSpawnResult {
                        thread_id,
                        session_configured: thread.session_configured(),
                        thread,
                        runtime_origin: crate::thread_manager::ThreadRuntimeOrigin::Existing,
                        setup_cleanup: None,
                    });
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
        if !config.ephemeral
            && matches!(
                session_source.as_ref(),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
            )
        {
            self.sync_durable_agent_nickname_reservations().await?;
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
        let _parent_lifecycle_guard = if let Some(parent_thread_id) = notification_source
            .as_ref()
            .and_then(SessionSource::parent_thread_id)
        {
            Some(state.acquire_live_agent_lifecycle(parent_thread_id).await?)
        } else {
            None
        };
        let observer_multi_agent_version =
            if let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                ..
            })) = notification_source.as_ref()
            {
                let parent_thread = state.get_thread(*parent_thread_id).await?;
                Some(response_observer_multi_agent_version(
                    &parent_thread,
                    options.response_observer,
                ))
            } else {
                None
            };

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
                    ThreadRuntimePublication::Deferred,
                ))
                .await?
            }
            (None, _, _) => Box::pin(state.spawn_new_thread(config.clone(), self.clone())).await?,
        };
        // The new ID becomes externally discoverable later in this function. Hold the same
        // lifecycle boundary used by send/resume until watcher registration, edge persistence,
        // and thread-created publication are complete so an immediate close cannot be overwritten
        // by the tail of spawn setup.
        let setup_cleanup = SetupCleanupGuard::new_with_agent_lifecycle(
            "spawn agent",
            Arc::clone(&state),
            new_thread.thread_id,
            {
                let control = self.clone();
                let state = Arc::clone(&state);
                let thread = Arc::clone(&new_thread.thread);
                let should_close_persisted_lifecycle = !config.ephemeral
                    && matches!(
                        notification_source.as_ref(),
                        Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
                    );
                async move {
                    let owns_runtime = state.thread_instance_is_current_or_pending(&thread).await;
                    let alias_cleanup = if should_close_persisted_lifecycle && owns_runtime {
                        control
                            .persist_agent_closed(thread.session.thread_id())
                            .await
                    } else {
                        Ok(())
                    };
                    let runtime_cleanup = control
                        .discard_unpublished_agent_instance(
                            &thread,
                            LiveAgentMetadataDisposition::Release,
                        )
                        .await;
                    alias_cleanup.and(runtime_cleanup)
                }
            },
        );
        let lifecycle_lock = state.agent_lifecycle_lock(new_thread.thread_id);
        let _lifecycle_guard = lifecycle_lock.lock_owned().await;
        agent_metadata.agent_id = Some(new_thread.thread_id);
        if matches!(
            notification_source.as_ref(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
        ) {
            new_thread.thread.ensure_rollout_materialized().await;
        }
        let persisted_alias = match self
            .persist_thread_spawn_for_source(
                new_thread.thread.as_ref(),
                new_thread.thread_id,
                notification_source.as_ref(),
                ThreadSpawnPersistence::New,
            )
            .await
        {
            Ok(alias) => alias,
            Err(err) => {
                if let Err(cleanup_err) = setup_cleanup.rollback().await {
                    return Err(CodexErr::Fatal(format!(
                        "{err}; failed to roll back child after alias persistence failed: {cleanup_err}"
                    )));
                }
                return Err(err);
            }
        };
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
        let mut post_admission_warning = None;
        let unpublished_spawn_result: CodexResult<()> = async {
            let _initial_user_input_permit =
                if matches!(&initial_input, SpawnInitialInput::UserInput { .. }) {
                    Some(
                        self.acquire_mailbox_submission_permit(new_thread.thread_id)
                            .await?,
                    )
                } else {
                    None
                };
            // Admission binding follows the observer contract, not the child's protocol. User
            // control and V1 tools can deliberately attach a durable observer to a V2 child.
            let _response_observation_transaction = if observer_multi_agent_version
                == Some(MultiAgentVersion::V1)
                && let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    ..
                })) = notification_source.as_ref()
            {
                let parent = state
                    .get_thread(*parent_thread_id)
                    .await?
                    .session
                    .presentation_id();
                Some(self.acquire_response_observation_transaction(parent).await)
            } else {
                None
            };
            // Attach before exposing the child or submitting its first input so an early failure
            // cannot publish a watcher-owned final-outcome presentation without a consumer.
            self.maybe_start_completion_watcher(
                &new_thread.thread,
                notification_source.clone(),
                child_reference,
                agent_metadata.agent_path.clone(),
                options.response_observation,
                options.response_observer,
                InitialTerminalObservation::FutureTurnsOnly,
            )
            .await?;

            match initial_input {
                SpawnInitialInput::None => {}
                SpawnInitialInput::UserInput {
                    input,
                    user_task_preview,
                    post_admission_failure,
                } => {
                    let observed_task_preview =
                        non_empty_task_message(render_input_preview(&input));
                    let send_result = self
                        .send_input_to_retained_thread(
                            new_thread.thread_id,
                            &state,
                            &new_thread.thread,
                            input,
                            TurnStartOptions {
                                parent_turn_id: options.parent_turn_id.clone(),
                                root_turn_id: options.root_turn_id.clone(),
                                cyber_access_program: options.cyber_access_program,
                                ..Default::default()
                            },
                            InputTurnAdmissionPolicy::AnyTurn,
                        )
                        .await;
                    let (_submission_id, resolution) = match send_result {
                        Ok(result) => result,
                        Err(err) => {
                            if observer_multi_agent_version == Some(MultiAgentVersion::V1)
                                && let Some(SessionSource::SubAgent(
                                    SubAgentSource::ThreadSpawn {
                                        parent_thread_id, ..
                                    },
                                )) = notification_source.as_ref()
                                && let Ok(parent_thread) =
                                    state.get_thread(*parent_thread_id).await
                            {
                                let parent = parent_thread.session.presentation_id();
                                let child = new_thread.thread.session.presentation_id();
                                self.rollback_response_observation_relationship_locked(
                                    parent,
                                    child,
                                    /*previous_relationship*/ None,
                                    /*target_turn_id*/ None,
                                    "failed to clean up response observation state after spawned turn admission failed",
                                )
                                .await?;
                            }
                            return Err(err);
                        }
                    };
                    if observer_multi_agent_version == Some(MultiAgentVersion::V1)
                        && let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            ..
                        })) = notification_source.as_ref()
                    {
                        let parent = state
                            .get_thread(*parent_thread_id)
                            .await?
                            .session
                            .presentation_id();
                        let child = new_thread.thread.session.presentation_id();
                        let observation_updates = if options.response_observation.commentary()
                            || options.response_observation.final_response()
                                != crate::agent::response_observation::FinalResponseObservation::None
                        {
                            self.bind_response_observation_turn_at_sequence(
                                parent,
                                child,
                                &resolution.target_turn_id,
                                ResponseObservationBinding::NextTurn,
                                Some((
                                    resolution.minimum_event_sequence,
                                    resolution.after_item_id.clone(),
                                )),
                                observed_task_preview,
                                ResponseObservationBindingPublication::Deferred,
                            );
                            self.response_observation_snapshots(parent, child)
                        } else {
                            self.response_observation_audit_snapshots(
                                parent,
                                child,
                                Some(resolution.target_turn_id.clone()),
                            )
                        };
                        if !self
                            .persist_response_observation_updates(parent, observation_updates)
                            .await
                        {
                            let message = "failed to persist spawned response observation state";
                            let rollback = self
                                .rollback_response_observation_relationship_locked(
                                    parent,
                                    child,
                                    /*previous_relationship*/ None,
                                    Some(resolution.target_turn_id.clone()),
                                    message,
                                )
                                .await;
                            match post_admission_failure {
                                SpawnPostAdmissionFailure::Strict => {
                                    rollback?;
                                    return Err(CodexErr::Fatal(message.to_string()));
                                }
                                SpawnPostAdmissionFailure::PreserveAdmittedUserWork => {
                                    let warning = post_admission_response_observation_warning(
                                        message,
                                        "child input",
                                        rollback,
                                    );
                                    tracing::warn!(
                                        child_thread_id = %new_thread.thread_id,
                                        warning,
                                        "user agent spawn committed without durable response observation"
                                    );
                                    post_admission_warning = Some(warning);
                                }
                            }
                        }
                    }
                    if post_admission_warning.is_none()
                        && let Some(user_task_preview) = user_task_preview
                        && let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            ..
                        })) = notification_source.as_ref()
                    {
                        let parent = state
                            .get_thread(*parent_thread_id)
                            .await?
                            .session
                            .presentation_id();
                        if let Err(err) = self
                            .persist_user_agent_task_context(
                                parent,
                                new_thread.thread_id,
                                user_task_preview,
                            )
                            .await
                        {
                            let message =
                                format!("failed to persist spawned user agent task context: {err}");
                            let rollback = self
                                .rollback_response_observation_relationship_locked(
                                    parent,
                                    new_thread.thread.session.presentation_id(),
                                    /*previous_relationship*/ None,
                                    Some(resolution.target_turn_id.clone()),
                                    &message,
                                )
                                .await;
                            match post_admission_failure {
                                SpawnPostAdmissionFailure::Strict => {
                                    rollback?;
                                    return Err(CodexErr::Fatal(message));
                                }
                                SpawnPostAdmissionFailure::PreserveAdmittedUserWork => {
                                    let warning = post_admission_response_observation_warning(
                                        &message,
                                        "child input",
                                        rollback,
                                    );
                                    tracing::warn!(
                                        child_thread_id = %new_thread.thread_id,
                                        warning,
                                        "user agent spawn committed without durable source task context"
                                    );
                                    post_admission_warning = Some(warning);
                                }
                            }
                        }
                    }
                    if observer_multi_agent_version == Some(MultiAgentVersion::V1)
                        && post_admission_warning.is_none()
                    {
                        // The explicit next-turn binding remains unpublished until task linkage is
                        // durable, so an immediate child response cannot outrun its explanation.
                        self.publish_response_observation_binding();
                    }
                }
                SpawnInitialInput::InterAgentCommunication(communication, context) => {
                    self.send_inter_agent_communication_after_capacity_check(
                        new_thread.thread_id,
                        &state,
                        communication,
                        context,
                        TurnStartOptions {
                            parent_turn_id: options.parent_turn_id.clone(),
                            root_turn_id: options.root_turn_id.clone(),
                            cyber_access_program: options.cyber_access_program,
                            ..Default::default()
                        },
                    )
                    .await?;
                }
            }
            state.publish_thread(&new_thread.thread).await?;
            Ok(())
        }
        .await;
        if let Err(err) = unpublished_spawn_result {
            if let Err(cleanup_err) = setup_cleanup.rollback().await {
                return Err(CodexErr::Fatal(format!(
                    "{err}; failed to roll back unpublished child setup: {cleanup_err}"
                )));
            }
            return Err(err);
        }
        setup_cleanup.disarm();

        // Announce the child only after its runtime, first input, and response-observation state
        // are durable. The alias and parent edge commit before watcher setup and publication.
        state.notify_thread_created(new_thread.thread_id);

        Ok(LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status: self.get_status(new_thread.thread_id).await,
            agent_ref: persisted_alias.map(|alias| alias.agent_ref),
            post_admission_warning,
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
    ) -> CodexResult<crate::thread_manager::ThreadSpawnResult> {
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
        let forked_rollout_items = state
            .load_agent_model_context(parent_thread_id, parent_history_mode)
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
                // Non-paginated checkpoints force the child to rebuild context regardless of the
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
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::AgentResponseObservation(_) => true,
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
                ThreadRuntimePublication::Deferred,
            )
            .await
    }

    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<ThreadId> {
        self.resume_agent_from_rollout_with_persistence(
            config,
            thread_id,
            session_source,
            ResumeAgentControlOptions {
                response_observation,
                response_observer: ResponseObserverKind::Native,
                initial_terminal_observation: InitialTerminalObservation::FutureTurnsOnly,
                durable_response_observer_source: None,
                initial_user_input: None,
                thread_spawn_persistence: ThreadSpawnPersistence::ControlledResume,
            },
        )
        .await
        .map(|outcome| outcome.thread_id)
    }

    pub(crate) async fn resume_agent_from_rollout_adopting(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
        expected_previous_session_id: Option<SessionId>,
        authored_selector: String,
    ) -> CodexResult<ThreadId> {
        self.resume_agent_from_rollout_with_persistence(
            config,
            thread_id,
            session_source,
            ResumeAgentControlOptions {
                response_observation,
                response_observer: ResponseObserverKind::Native,
                initial_terminal_observation: InitialTerminalObservation::FutureTurnsOnly,
                durable_response_observer_source: None,
                initial_user_input: None,
                thread_spawn_persistence: ThreadSpawnPersistence::Transfer {
                    expected_previous_session_id,
                    reserved_descendant_thread_ids: None,
                    authored_selector,
                },
            },
        )
        .await
        .map(|outcome| outcome.thread_id)
    }

    pub(crate) async fn resume_user_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<Option<AgentAlias>> {
        let durable_response_observer_source = session_source.clone();
        self.resume_agent_from_rollout_with_persistence(
            config,
            thread_id,
            session_source,
            ResumeAgentControlOptions {
                response_observation,
                response_observer: ResponseObserverKind::Durable,
                initial_terminal_observation:
                    InitialTerminalObservation::ReconcileOrObserveNextFrom(AgentStatus::NotFound),
                durable_response_observer_source: Some(durable_response_observer_source),
                initial_user_input: None,
                thread_spawn_persistence: ThreadSpawnPersistence::ControlledResume,
            },
        )
        .await
        .map(|outcome| outcome.persisted_alias)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn resume_user_agent_from_rollout_adopting(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        response_observation: ResponseObservationPolicy,
        expected_previous_session_id: Option<SessionId>,
        authored_selector: String,
    ) -> CodexResult<Option<AgentAlias>> {
        let durable_response_observer_source = session_source.clone();
        self.resume_agent_from_rollout_with_persistence(
            config,
            thread_id,
            session_source,
            ResumeAgentControlOptions {
                response_observation,
                response_observer: ResponseObserverKind::Durable,
                initial_terminal_observation:
                    InitialTerminalObservation::ReconcileOrObserveNextFrom(AgentStatus::NotFound),
                durable_response_observer_source: Some(durable_response_observer_source),
                initial_user_input: None,
                thread_spawn_persistence: ThreadSpawnPersistence::Transfer {
                    expected_previous_session_id,
                    reserved_descendant_thread_ids: None,
                    authored_selector,
                },
            },
        )
        .await
        .map(|outcome| outcome.persisted_alias)
    }

    pub(crate) async fn resume_agent_from_rollout_with_user_input(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        admission: ResumeUserInputAdmission,
    ) -> CodexResult<ResponseObservationSubmission> {
        self.resume_agent_from_rollout_with_persistence(
            config,
            thread_id,
            session_source,
            ResumeAgentControlOptions {
                response_observation: ResponseObservationPolicy::from_parts(
                    /*commentary*/ false,
                    FinalResponseObservation::None,
                ),
                response_observer: ResponseObserverKind::Durable,
                initial_terminal_observation: InitialTerminalObservation::FutureTurnsOnly,
                durable_response_observer_source: None,
                initial_user_input: Some(admission),
                thread_spawn_persistence: ThreadSpawnPersistence::ControlledResume,
            },
        )
        .await?
        .initial_submission
        .ok_or_else(|| {
            CodexErr::Fatal("resumed agent prompt did not produce a submission".to_string())
        })
    }

    async fn resume_agent_from_rollout_with_persistence(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        options: ResumeAgentControlOptions,
    ) -> CodexResult<ResumeAgentControlOutcome> {
        let ResumeAgentControlOptions {
            response_observation,
            response_observer,
            initial_terminal_observation,
            durable_response_observer_source,
            initial_user_input,
            mut thread_spawn_persistence,
        } = options;
        let transfer_previous_session_id = match &thread_spawn_persistence {
            ThreadSpawnPersistence::Transfer {
                expected_previous_session_id,
                ..
            } => Some(*expected_previous_session_id),
            ThreadSpawnPersistence::New
            | ThreadSpawnPersistence::Resume
            | ThreadSpawnPersistence::ControlledResume => None,
        };
        let transfers_ownership = matches!(
            &thread_spawn_persistence,
            ThreadSpawnPersistence::Transfer { .. }
        );
        let defers_response_observer =
            durable_response_observer_source.is_some() || transfers_ownership;
        let deferred_response_observer_source =
            durable_response_observer_source.unwrap_or_else(|| session_source.clone());
        let (
            resume_response_observation,
            resume_response_observer,
            resume_initial_terminal_observation,
            post_resume_observation,
        ) = if defers_response_observer {
            (
                ResponseObservationPolicy::from_parts(
                    /*commentary*/ false,
                    FinalResponseObservation::None,
                ),
                ResponseObserverKind::Native,
                InitialTerminalObservation::FutureTurnsOnly,
                Some(DeferredResumeResponseObserver {
                    source: deferred_response_observer_source,
                    response_observation,
                    response_observer,
                    initial_terminal_observation,
                }),
            )
        } else {
            (
                response_observation,
                response_observer,
                initial_terminal_observation,
                None,
            )
        };
        let response_observer_source = session_source.clone();
        let session_source = self
            .canonical_same_root_resume_source(thread_id, session_source, &thread_spawn_persistence)
            .await?;
        let resumes_control_root = self
            .bound_session_id()
            .is_some_and(|session_id| thread_id == ThreadId::from(session_id));
        // The supplied source identifies the response observer, but Main remains the topology
        // root when a loaded child asks to resume it.
        let root_depth = if resumes_control_root {
            0
        } else {
            thread_spawn_depth(&session_source).unwrap_or(0)
        };
        let target_parent_thread_id = if resumes_control_root {
            None
        } else {
            session_source.parent_thread_id()
        };
        if target_parent_thread_id == Some(thread_id) {
            return Err(CodexErr::InvalidRequest(format!(
                "agent {thread_id} cannot be adopted beneath itself"
            )));
        }
        let state = self.upgrade()?;
        // Same-root resume follows the persisted parent-before-child order. A transfer cannot lock
        // its requested destination parent yet: that parent may be a member of the old subtree and
        // must be rejected before taking locks in the opposite order.
        let mut _parent_lifecycle_guard = if transfers_ownership {
            None
        } else if let Some(parent_thread_id) = target_parent_thread_id {
            Some(state.acquire_live_agent_lifecycle(parent_thread_id).await?)
        } else {
            None
        };
        let (resumed_thread, resumed_multi_agent_version, initial_submission, persisted_alias) = {
            let lifecycle_lock = state.agent_lifecycle_lock(thread_id);
            let _lifecycle_guard = lifecycle_lock.lock_owned().await;
            if matches!(
                &thread_spawn_persistence,
                ThreadSpawnPersistence::ControlledResume
            ) {
                self.require_current_agent_ownership(thread_id).await?;
            }
            let mut transfer_descendant_ids = Vec::new();
            let mut locked_transfer_thread_ids = HashSet::from([thread_id]);
            let mut _transfer_descendant_guards = Vec::new();
            let mut _transfer_writer_reservation = None;
            if transfers_ownership {
                if state.get_thread(thread_id).await.is_ok() {
                    return Err(CodexErr::InvalidRequest(format!(
                        "agent {thread_id} is live under another root; close it before adoption"
                    )));
                }
                if let Some(agent_graph_store) = state.agent_graph_store() {
                    // Match subtree-close lock ordering. Holding every persisted descendant
                    // boundary prevents the former owner from reopening or extending the graph
                    // while the alias transaction replaces its root edge.
                    let mut parents = VecDeque::from([thread_id]);
                    while let Some(subtree_parent_thread_id) = parents.pop_front() {
                        let child_ids = agent_graph_store
                            .list_thread_spawn_children(
                                subtree_parent_thread_id,
                                /*status_filter*/ None,
                            )
                            .await
                            .map_err(|err| {
                                CodexErr::Fatal(format!(
                                    "failed to load adoption subtree for {thread_id}: {err}"
                                ))
                            })?;
                        for descendant_id in child_ids {
                            if locked_transfer_thread_ids.insert(descendant_id) {
                                if target_parent_thread_id == Some(descendant_id) {
                                    return Err(CodexErr::InvalidRequest(format!(
                                        "agent {thread_id} cannot be adopted beneath its own \
                                         descendant {descendant_id}"
                                    )));
                                }
                                let guard =
                                    state.agent_lifecycle_lock(descendant_id).lock_owned().await;
                                transfer_descendant_ids.push(descendant_id);
                                _transfer_descendant_guards.push(guard);
                                parents.push_back(descendant_id);
                            } else if descendant_id == thread_id {
                                return Err(CodexErr::InvalidRequest(format!(
                                    "agent {thread_id} belongs to a cyclic persisted spawn graph"
                                )));
                            }
                        }
                    }
                }
                let mut live_descendant_id = None;
                for descendant_id in transfer_descendant_ids.iter().copied() {
                    if state.get_thread(descendant_id).await.is_ok() {
                        live_descendant_id = Some(descendant_id);
                        break;
                    }
                }
                if let Some(live_descendant_id) = live_descendant_id {
                    return Err(CodexErr::InvalidRequest(format!(
                        "agent {thread_id} has live descendant {live_descendant_id}; close the subtree before adoption"
                    )));
                }
                // Process-local lifecycle locks cannot see a writer held by another app-server.
                // Reserve every descendant rollout before the alias transaction so ownership
                // cannot move while any process can still append to the old subtree.
                _transfer_writer_reservation = Some(
                    state
                        .reserve_thread_writers(transfer_descendant_ids.clone())
                        .await?,
                );
                if let ThreadSpawnPersistence::Transfer {
                    reserved_descendant_thread_ids,
                    ..
                } = &mut thread_spawn_persistence
                {
                    *reserved_descendant_thread_ids = Some(transfer_descendant_ids.clone());
                }
                // The old subtree is now stable and known not to contain the destination parent.
                // Keep that parent live until alias transfer and runtime publication complete.
                _parent_lifecycle_guard = if let Some(parent_thread_id) = target_parent_thread_id {
                    Some(state.acquire_live_agent_lifecycle(parent_thread_id).await?)
                } else {
                    None
                };
            }
            let ResumeSingleAgentOutcome {
                thread: resumed_thread,
                multi_agent_version: resumed_multi_agent_version,
                runtime_origin,
                persisted_alias,
                mut setup_cleanup,
            } = Box::pin(
                self.resume_single_agent_from_rollout(ResumeSingleAgentOptions {
                    config: config.clone(),
                    thread_id,
                    session_source,
                    response_observer_source,
                    initial_history_override: None,
                    client_mcp_extensions_override: None,
                    response_observation: resume_response_observation,
                    response_observer: resume_response_observer,
                    initial_terminal_observation: resume_initial_terminal_observation,
                    thread_spawn_persistence,
                }),
            )
            .await?;
            if let Some(previous_session_id) = transfer_previous_session_id {
                // The alias transaction above is the exclusive ownership commit. Invalidate the
                // complete locked subtree before any destination watcher can capture the new
                // generation or the deferred runtime can become externally visible.
                self.invalidate_transferred_subtree_response_observers(
                    previous_session_id,
                    thread_id,
                    &transfer_descendant_ids,
                )
                .await;
            }
            let mut response_observer_cleanup = None;
            let post_resume_result: CodexResult<Option<ResponseObservationSubmission>> = async {
                if let Some(DeferredResumeResponseObserver {
                    source,
                    response_observation,
                    response_observer,
                    initial_terminal_observation,
                }) = post_resume_observation
                {
                    let response_observer_endpoint = match &source {
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            ..
                        }) => state
                            .get_thread_including_pending(*parent_thread_id)
                            .await
                            .ok()
                            .map(|parent_thread| {
                                (
                                    parent_thread.session.presentation_id(),
                                    resumed_thread.session.presentation_id(),
                                )
                            }),
                        _ => None,
                    };
                    if transfers_ownership
                        && let Some((parent, child)) = response_observer_endpoint
                        && (self
                            .response_watcher_registration_id(parent, child)
                            .is_some()
                            || self
                                .response_observation_relationship_snapshot(parent, child)
                                .is_some())
                    {
                        return Err(CodexErr::Fatal(format!(
                            "destination response observation already exists for transferred agent \
                             {thread_id}"
                        )));
                    }
                    let _response_observation_permit =
                        if let Some((parent, _)) = response_observer_endpoint {
                            Some(self.acquire_response_observation_transaction(parent).await)
                        } else {
                            None
                        };
                    let watcher_setup_sink = if transfers_ownership
                        && let Some((parent, child)) = response_observer_endpoint
                    {
                        let setup_sink =
                            Arc::new(std::sync::Mutex::new(None::<CompletionWatcherSetup>));
                        let cleanup_sink = Arc::clone(&setup_sink);
                        let control = self.clone();
                        response_observer_cleanup = Some(SetupCleanupGuard::new(
                            "resume response observer",
                            async move {
                                let setup = cleanup_sink
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .take();
                                let Some(setup) = setup else {
                                    return Ok(());
                                };
                                control
                                    .rollback_installed_response_observer_if_current(
                                        parent,
                                        child,
                                        setup.registration_id,
                                        setup.target_turn_ids,
                                    )
                                    .await
                            },
                        ));
                        Some(setup_sink)
                    } else {
                        None
                    };
                    let metadata = self.get_agent_metadata(thread_id).unwrap_or_default();
                    let child_reference = metadata
                        .agent_path
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| thread_id.to_string());
                    self.maybe_start_completion_watcher_tracked(
                        &resumed_thread,
                        Some(source),
                        child_reference,
                        metadata.agent_path,
                        response_observation,
                        response_observer,
                        initial_terminal_observation,
                        watcher_setup_sink.as_ref(),
                    )
                    .await?;
                }
                let initial_submission = if let Some(admission) = initial_user_input {
                    let ResumeUserInputAdmission {
                        input,
                        observer,
                        response_observation,
                        admission_policy,
                        task_preview,
                    } = admission;
                    Some(
                        self.send_input_observing_response_to_retained_thread_locked(
                            thread_id,
                            &state,
                            &resumed_thread,
                            ObservedUserInputAdmission {
                                input,
                                start_options: TurnStartOptions::default(),
                                observer,
                                response_observation,
                                admission_policy,
                                task_context: task_preview.map_or(
                                    ObservedInputTaskContext::None,
                                    ObservedInputTaskContext::UserAuthored,
                                ),
                            },
                        )
                        .await?,
                    )
                } else {
                    None
                };
                if runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Created {
                    state.publish_thread(&resumed_thread).await?;
                }
                Ok(initial_submission)
            }
            .await;
            let initial_submission = match post_resume_result {
                Ok(initial_submission) => initial_submission,
                Err(err) => {
                    let mut cleanup_errors = Vec::new();
                    if let Some(response_observer_cleanup) = response_observer_cleanup.take()
                        && let Err(cleanup_err) = response_observer_cleanup.rollback().await
                    {
                        cleanup_errors.push(format!(
                            "failed to roll back destination response observer: {cleanup_err}"
                        ));
                    }
                    if let Some(setup_cleanup) = setup_cleanup.take()
                        && let Err(cleanup_err) = setup_cleanup.rollback().await
                    {
                        cleanup_errors
                            .push(format!("failed to roll back reopened agent: {cleanup_err}"));
                    }
                    if !cleanup_errors.is_empty() {
                        return Err(CodexErr::Fatal(format!(
                            "{err}; setup cleanup also failed: {}",
                            cleanup_errors.join("; ")
                        )));
                    }
                    return Err(err);
                }
            };
            if let Some(response_observer_cleanup) = response_observer_cleanup.take() {
                response_observer_cleanup.disarm();
            }
            if let Some(setup_cleanup) = setup_cleanup.take() {
                setup_cleanup.disarm();
            }
            if runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Created {
                state.notify_thread_created(thread_id);
            }
            (
                resumed_thread,
                resumed_multi_agent_version,
                initial_submission,
                persisted_alias,
            )
        };
        let resumed_thread_id = resumed_thread.session.thread_id();
        if config.multi_agent_version_from_features() == MultiAgentVersion::V2
            || resumed_multi_agent_version == MultiAgentVersion::V2
        {
            return Ok(ResumeAgentControlOutcome {
                thread_id: resumed_thread_id,
                initial_submission,
                persisted_alias,
            });
        }
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(ResumeAgentControlOutcome {
                thread_id: resumed_thread_id,
                initial_submission,
                persisted_alias,
            });
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
                let _parent_lifecycle_guard = match state
                    .acquire_live_agent_lifecycle(parent_thread_id)
                    .await
                {
                    Ok(guard) => guard,
                    Err(err) => {
                        warn!(
                            "failed to resume descendant thread {child_thread_id} because its parent {parent_thread_id} is no longer live: {err}"
                        );
                        continue;
                    }
                };
                let lifecycle_lock = state.agent_lifecycle_lock(child_thread_id);
                let _lifecycle_guard = lifecycle_lock.lock_owned().await;
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
                    let response_observer_source = child_session_source.clone();
                    match Box::pin(self.resume_single_agent_from_rollout(
                        ResumeSingleAgentOptions {
                            config: config.clone(),
                            thread_id: child_thread_id,
                            session_source: child_session_source,
                            response_observer_source,
                            initial_history_override: None,
                            client_mcp_extensions_override: None,
                            response_observation: ResponseObservationPolicy::default(),
                            response_observer: ResponseObserverKind::Native,
                            initial_terminal_observation:
                                InitialTerminalObservation::FutureTurnsOnly,
                            thread_spawn_persistence: ThreadSpawnPersistence::Resume,
                        },
                    ))
                    .await
                    {
                        Ok(mut resumed) => {
                            if resumed.runtime_origin
                                == crate::thread_manager::ThreadRuntimeOrigin::Created
                            {
                                if let Err(err) = state.publish_thread(&resumed.thread).await {
                                    if let Some(setup_cleanup) = resumed.setup_cleanup.take()
                                        && let Err(cleanup_err) = setup_cleanup.rollback().await
                                    {
                                        warn!(
                                            "failed to discard unpublished descendant \
                                             {child_thread_id} after publication failed: \
                                             {err}; {cleanup_err}"
                                        );
                                    } else {
                                        warn!(
                                            "failed to publish resumed descendant thread \
                                             {child_thread_id}: {err}"
                                        );
                                    }
                                    false
                                } else {
                                    if let Some(setup_cleanup) = resumed.setup_cleanup.take() {
                                        setup_cleanup.disarm();
                                    }
                                    state.notify_thread_created(child_thread_id);
                                    true
                                }
                            } else {
                                if let Some(setup_cleanup) = resumed.setup_cleanup.take() {
                                    setup_cleanup.disarm();
                                }
                                true
                            }
                        }
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

        Ok(ResumeAgentControlOutcome {
            thread_id: resumed_thread_id,
            initial_submission,
            persisted_alias,
        })
    }

    async fn canonical_same_root_resume_source(
        &self,
        thread_id: ThreadId,
        session_source: SessionSource,
        thread_spawn_persistence: &ThreadSpawnPersistence,
    ) -> CodexResult<SessionSource> {
        let Some(session_id) = self.bound_session_id() else {
            return Ok(session_source);
        };
        let preserves_existing_parent = match thread_spawn_persistence {
            ThreadSpawnPersistence::Resume | ThreadSpawnPersistence::ControlledResume => true,
            ThreadSpawnPersistence::Transfer {
                expected_previous_session_id,
                ..
            } => *expected_previous_session_id == Some(session_id),
            ThreadSpawnPersistence::New => false,
        };
        if !preserves_existing_parent {
            return Ok(session_source);
        }

        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(session_source);
        };
        if !agent_graph_store.supports_agent_aliases() {
            return Ok(session_source);
        }
        let direct_parent_thread_id = agent_graph_store
            .find_thread_spawn_parent(thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to load persisted parent for resumed agent {thread_id}: {err}"
                ))
            })?;
        let Some(direct_parent_thread_id) = direct_parent_thread_id else {
            return Ok(session_source);
        };
        let durable_agent_nickname = agent_graph_store
            .find_agent_alias_by_thread(session_id, thread_id)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to load durable identity for resumed agent {thread_id}: {err}"
                ))
            })?
            .map(|alias| alias.nickname);

        let root_thread_id = ThreadId::from(session_id);
        let mut ancestor_thread_id = direct_parent_thread_id;
        let mut depth = 1usize;
        let mut visited = HashSet::from([thread_id]);
        while ancestor_thread_id != root_thread_id {
            if !visited.insert(ancestor_thread_id) {
                return Err(CodexErr::InvalidRequest(format!(
                    "agent {thread_id} belongs to a cyclic persisted spawn graph"
                )));
            }
            ancestor_thread_id = agent_graph_store
                .find_thread_spawn_parent(ancestor_thread_id)
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to load persisted ancestry for resumed agent {thread_id}: {err}"
                    ))
                })?
                .ok_or_else(|| {
                    CodexErr::Fatal(format!(
                        "persisted ancestry for resumed agent {thread_id} does not reach Main"
                    ))
                })?;
            depth = depth.checked_add(1).ok_or_else(|| {
                CodexErr::Fatal(format!(
                    "persisted ancestry for resumed agent {thread_id} exceeds supported depth"
                ))
            })?;
        }
        let depth = i32::try_from(depth).map_err(|_| {
            CodexErr::Fatal(format!(
                "persisted ancestry for resumed agent {thread_id} exceeds supported depth"
            ))
        })?;
        let (agent_path, agent_nickname, agent_role) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                agent_path,
                agent_nickname,
                agent_role,
                ..
            }) => (agent_path, agent_nickname, agent_role),
            SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Exec
            | SessionSource::Mcp
            | SessionSource::Custom(_)
            | SessionSource::Internal(_)
            | SessionSource::SubAgent(_)
            | SessionSource::Unknown => (None, None, None),
        };
        Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: direct_parent_thread_id,
            depth,
            agent_path,
            agent_nickname: durable_agent_nickname.unwrap_or(agent_nickname),
            agent_role,
        }))
    }

    pub(crate) async fn canonical_controlled_resume_source(
        &self,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<SessionSource> {
        self.canonical_same_root_resume_source(
            thread_id,
            session_source,
            &ThreadSpawnPersistence::ControlledResume,
        )
        .await
    }

    /// Activate a standalone resume under its current durable owner before runtime publication.
    ///
    /// The caller must hold the owner, direct-parent, and target lifecycle boundaries together
    /// with an exclusive live-writer lease.
    pub(crate) async fn activate_controlled_resume_alias(
        &self,
        thread: &CodexThread,
        session_source: &SessionSource,
    ) -> CodexResult<Option<AgentAlias>> {
        let thread_id = thread.session.thread_id();
        self.require_current_agent_ownership(thread_id).await?;
        self.persist_thread_spawn_for_source(
            thread,
            thread_id,
            Some(session_source),
            ThreadSpawnPersistence::ControlledResume,
        )
        .await
    }

    /// Reopen a V2 child while the caller holds its direct parent's lifecycle guard.
    pub(crate) async fn resume_v2_agent_from_history(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
        initial_history: InitialHistory,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<crate::thread_manager::ThreadSpawnResult> {
        let response_observer_source = session_source.clone();
        let ResumeSingleAgentOutcome {
            thread: resumed_thread,
            multi_agent_version,
            runtime_origin,
            mut setup_cleanup,
            ..
        } = Box::pin(
            self.resume_single_agent_from_rollout(ResumeSingleAgentOptions {
                config,
                thread_id,
                session_source,
                response_observer_source,
                initial_history_override: Some(initial_history),
                client_mcp_extensions_override: Some(client_mcp_extensions),
                response_observation: ResponseObservationPolicy::default(),
                response_observer: ResponseObserverKind::Native,
                initial_terminal_observation: InitialTerminalObservation::FutureTurnsOnly,
                thread_spawn_persistence: ThreadSpawnPersistence::Resume,
            }),
        )
        .await?;
        if multi_agent_version != MultiAgentVersion::V2 {
            if let Some(setup_cleanup) = setup_cleanup.take() {
                setup_cleanup.rollback().await?;
            }
            return Err(CodexErr::InvalidRequest(format!(
                "persisted spawned child {thread_id} is not running Multi-Agent V2"
            )));
        }
        Ok(crate::thread_manager::ThreadSpawnResult {
            thread_id,
            session_configured: resumed_thread.session_configured(),
            thread: resumed_thread,
            runtime_origin,
            setup_cleanup,
        })
    }

    async fn resume_single_agent_from_rollout(
        &self,
        options: ResumeSingleAgentOptions,
    ) -> CodexResult<ResumeSingleAgentOutcome> {
        let ResumeSingleAgentOptions {
            mut config,
            thread_id,
            session_source,
            response_observer_source,
            initial_history_override,
            client_mcp_extensions_override,
            response_observation,
            response_observer,
            initial_terminal_observation,
            thread_spawn_persistence,
        } = options;
        let state = self.upgrade()?;
        if !config.ephemeral && session_source.is_non_root_agent() {
            self.sync_durable_agent_nickname_reservations().await?;
        }
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
                (None, None, None, stored_parent_thread_id, initial_history)
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
                let history = state
                    .load_agent_model_context(thread_id, stored_thread.history_mode)
                    .await?
                    .ok_or(CodexErr::ThreadNotFound(thread_id))?;
                (
                    resumed_agent_path,
                    stored_thread.agent_nickname,
                    stored_thread.agent_role,
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
        let persisted_session_id =
            initial_history
                .get_rollout_items()
                .iter()
                .rev()
                .find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == thread_id => {
                        Some(meta_line.meta.session_id)
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
        let bound_session_id = self.bound_session_id();
        let ownership_changed_since_rollout =
            persisted_session_id.is_some_and(|session_id| Some(session_id) != bound_session_id);
        let session_source =
            if bound_session_id.is_some_and(|session_id| thread_id == ThreadId::from(session_id)) {
                // A child can explicitly reopen its unloaded Main thread. Main keeps its persisted
                // root source and must not be reclassified as a child of the caller merely because
                // the caller owns response observation for the resumed turn.
                canonical_session_source.clone().unwrap_or(session_source)
            } else {
                session_source
            };
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
        let adopts_foreign_thread = matches!(
            &thread_spawn_persistence,
            ThreadSpawnPersistence::Transfer {
                expected_previous_session_id,
                ..
            } if bound_session_id
                .is_none_or(|session_id| *expected_previous_session_id != Some(session_id))
        );
        let destination_alias = if adopts_foreign_thread || ownership_changed_since_rollout {
            self.find_session_agent_alias(thread_id).await?
        } else {
            None
        };
        // The outer option distinguishes an absent alias from a durable alias whose nickname was
        // intentionally omitted, for example after a descendant collision during subtree transfer.
        let durable_agent_nickname = if adopts_foreign_thread {
            None
        } else {
            self.find_session_agent_alias(thread_id)
                .await?
                .map(|alias| alias.nickname)
        };
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let resume_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && is_v2_resident_session_source(&session_source);
        let reservation_max_threads = if resume_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = self.state.reserve_spawn_slot(reservation_max_threads)?;
        let (session_source, mut agent_metadata, register_resumed_agent) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                agent_nickname,
            }) => {
                if adopts_foreign_thread || ownership_changed_since_rollout {
                    // A foreign or already-transferred rollout keeps its historical source in
                    // persisted history, but its live identity must belong to the current graph.
                    // Reusing the old path or nickname can collide with an existing destination
                    // child, and retaining a standalone root source would skip ownership-aware
                    // response observation.
                    let restored_role = canonical_session_source
                        .as_ref()
                        .and_then(SessionSource::get_agent_role)
                        .or(resumed_agent_role);
                    let (session_source, metadata) = self.prepare_thread_spawn(
                        &mut reservation,
                        &config,
                        parent_thread_id,
                        depth,
                        agent_path,
                        agent_role.or(restored_role),
                        destination_alias.and_then(|alias| alias.nickname),
                    )?;
                    (session_source, metadata, true)
                } else if let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    agent_path,
                    agent_role,
                    agent_nickname,
                    ..
                })) = canonical_session_source
                {
                    let agent_nickname = durable_agent_nickname.clone().unwrap_or(agent_nickname);
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
                        durable_agent_nickname
                            .clone()
                            .unwrap_or_else(|| agent_nickname.or(resumed_agent_nickname)),
                    )?;
                    (session_source, metadata, true)
                } else {
                    // A SubAgent resume request is an explicit current control relationship even
                    // when an old rollout predates persisted child metadata. Keep the historical
                    // root source in its rollout, but do not reopen it as an unowned standalone
                    // runtime and thereby skip the caller's watcher and parent edge.
                    let (session_source, metadata) = self.prepare_thread_spawn(
                        &mut reservation,
                        &config,
                        parent_thread_id,
                        depth,
                        agent_path,
                        agent_role,
                        durable_agent_nickname.unwrap_or(agent_nickname),
                    )?;
                    (session_source, metadata, true)
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
            .inherited_environments_for_resume(&state, Some(&session_source), &config)
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, Some(&session_source), &config)
            .await;
        let previous_persisted_alias = if !config.ephemeral && session_source.is_non_root_agent() {
            match state.agent_graph_store() {
                Some(agent_graph_store) if agent_graph_store.supports_agent_aliases() => {
                    agent_graph_store
                        .find_current_agent_alias_by_thread(thread_id)
                        .await
                        .map_err(|err| {
                            CodexErr::Fatal(format!(
                                "failed to snapshot durable lifecycle before resuming agent \
                                     {thread_id}: {err}"
                            ))
                        })?
                }
                Some(_) | None => None,
            }
        } else {
            None
        };
        let previous_live_metadata = self.state.agent_metadata_for_thread(thread_id);
        agent_metadata.agent_id = Some(thread_id);
        let attempted_live_metadata = agent_metadata.clone();

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
                runtime_publication: ThreadRuntimePublication::Deferred,
            })
            .await?;
        let runtime_origin = resumed_thread.runtime_origin;
        let mut setup_cleanup = Some(SetupCleanupGuard::new_with_agent_lifecycle(
            "resume agent",
            Arc::clone(&state),
            thread_id,
            {
                let control = self.clone();
                let state = Arc::clone(&state);
                let thread = Arc::clone(&resumed_thread.thread);
                let previous_persisted_alias = previous_persisted_alias.clone();
                let previous_live_metadata = previous_live_metadata.clone();
                let attempted_live_metadata = attempted_live_metadata.clone();
                let graph = state.agent_graph_store();
                let should_restore_persisted_lifecycle =
                    !config.ephemeral && notification_source.is_non_root_agent();
                async move {
                    let owns_runtime = state.thread_instance_is_current_or_pending(&thread).await;
                    let lifecycle_restore = if should_restore_persisted_lifecycle && owns_runtime {
                        match (graph, previous_persisted_alias) {
                            (Some(graph), Some(previous_alias))
                                if graph.supports_agent_aliases()
                                    && previous_alias.session_id == control.session_id() =>
                            {
                                let status = agent_alias_lifecycle_status(previous_alias.state);
                                match status {
                                    Some(status) => graph
                                        .set_agent_lifecycle_state(
                                            previous_alias.session_id,
                                            previous_alias.thread_id,
                                            status,
                                        )
                                        .await
                                        .map_err(|err| {
                                            CodexErr::Fatal(format!(
                                                "failed to restore resumed agent lifecycle: {err}"
                                            ))
                                        })
                                        .and_then(|restored| {
                                            if restored {
                                                Ok(())
                                            } else {
                                                Err(CodexErr::Fatal(format!(
                                                    "resumed agent lifecycle disappeared for {}",
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
                            (Some(graph), Some(_previous_alias))
                                if graph.supports_agent_aliases() =>
                            {
                                // Ownership transfer is the durable commit point for adoption. A
                                // later watcher, prompt, publication, or caller-cancellation failure
                                // unloads the attempted runtime but intentionally does not steal the
                                // subtree back from its new root. The target remains explicitly
                                // resumable under that owner.
                                Ok(())
                            }
                            (Some(_) | None, Some(_) | None) => {
                                control
                                    .persist_agent_closed(thread.session.thread_id())
                                    .await
                            }
                        }
                    } else {
                        Ok(())
                    };
                    let runtime_cleanup =
                        if runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Created {
                            control
                                .discard_unpublished_agent_instance(
                                    &thread,
                                    if previous_live_metadata.is_some() {
                                        LiveAgentMetadataDisposition::Preserve
                                    } else {
                                        LiveAgentMetadataDisposition::Release
                                    },
                                )
                                .await
                        } else {
                            Ok(())
                        };
                    let metadata_restore = if owns_runtime {
                        match previous_live_metadata {
                            Some(previous_live_metadata) => control
                                .restore_agent_metadata_if_current(
                                    thread.session.thread_id(),
                                    &attempted_live_metadata,
                                    previous_live_metadata,
                                )
                                .map(drop),
                            None => {
                                let _ = control.clear_agent_metadata_if_current(
                                    thread.session.thread_id(),
                                    &attempted_live_metadata,
                                );
                                Ok(())
                            }
                        }
                    } else {
                        Ok(())
                    };
                    lifecycle_restore.and(runtime_cleanup).and(metadata_restore)
                }
            },
        ));
        let mut registered_by_attempt = false;
        let resumed_setup_result: CodexResult<Option<AgentAlias>> = async {
            if multi_agent_version == MultiAgentVersion::V2 {
                self.validate_loaded_v2_agent(&resumed_thread.thread, Some(&notification_source))?;
                self.validate_loaded_rollout_path(
                    &resumed_thread.thread,
                    expected_rollout_path.as_deref(),
                )?;
            }
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| resumed_thread.thread_id.to_string());
            let _response_observation_permit = if resumed_thread.thread.multi_agent_version()
                != Some(MultiAgentVersion::V2)
                && let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                }) = &response_observer_source
            {
                let parent = state
                    .get_thread(*parent_thread_id)
                    .await?
                    .session
                    .presentation_id();
                Some(self.acquire_response_observation_transaction(parent).await)
            } else {
                None
            };
            self.maybe_start_completion_watcher(
                &resumed_thread.thread,
                Some(response_observer_source),
                child_reference,
                agent_metadata.agent_path.clone(),
                response_observation,
                response_observer,
                initial_terminal_observation,
            )
            .await?;
            let persisted_alias = self
                .persist_thread_spawn_for_source(
                    resumed_thread.thread.as_ref(),
                    resumed_thread.thread_id,
                    Some(&notification_source),
                    thread_spawn_persistence,
                )
                .await?;
            if register_resumed_agent {
                registered_by_attempt = reservation.commit_if_absent(agent_metadata);
            }
            if let Some(residency_slot) = residency_slot {
                residency_slot.commit(resumed_thread.thread_id);
            }
            Ok(persisted_alias)
        }
        .await;
        let persisted_alias = match resumed_setup_result {
            Ok(persisted_alias) => persisted_alias,
            Err(err) => {
                if let Some(setup_cleanup) = setup_cleanup.take()
                    && let Err(cleanup_err) = setup_cleanup.rollback().await
                {
                    return Err(CodexErr::Fatal(format!(
                        "{err}; failed to roll back resumed agent setup: {cleanup_err}"
                    )));
                }
                if runtime_origin == crate::thread_manager::ThreadRuntimeOrigin::Existing
                    && registered_by_attempt
                {
                    self.state.release_spawned_thread(resumed_thread.thread_id);
                }
                return Err(err);
            }
        };
        Ok(ResumeSingleAgentOutcome {
            thread: resumed_thread.thread,
            multi_agent_version,
            runtime_origin,
            persisted_alias,
            setup_cleanup,
        })
    }
}
