use super::delivery::CapturedRecipient;
use super::residency::is_v2_resident_session_source;
use super::spawn_guard::PendingSpawn;
use super::spawn_guard::SpawnOwner;
use super::spawn_telemetry::SpawnMeasurements;
use super::spawn_telemetry::record_spawn_success;
use super::*;
use crate::agent::history::prepare_forked_history;
use crate::agent::types::AgentMetadata;
use crate::agent::types::LiveAgent;
use crate::agent::types::SpawnAgentOptions;
use crate::codex_thread::CodexThread;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::combine_selected_capability_roots;
use crate::thread_manager::StartThreadOptions;
use codex_features::Feature;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::error::AgentErrorContext;
use std::time::Instant;

const AGENT_NAMES: &str = include_str!("../../../assets/agent/agent_names.txt");

/// Initial input delivered after a spawned agent acquires execution capacity.
///
/// V2 communication spawns keep the communication and its context paired so centralized
/// submission and lifecycle logging cannot receive one without the other. Other spawn sources
/// provide user input directly, making an uncontextualized inter-agent communication
/// unrepresentable.
#[allow(clippy::large_enum_variant)]
pub(super) enum SpawnInitialInput {
    UserInput(Vec<UserInput>),
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext),
}

pub(in crate::agent) async fn effective_spawn_config(
    thread: &CodexThread,
) -> CodexResult<ThreadConfigSnapshot> {
    let mut config = thread.config_snapshot().await;
    let model_info = thread
        .thread_extension_data()
        .get::<codex_protocol::openai_models::ModelInfo>()
        .ok_or_else(|| CodexErr::Fatal("spawned thread is missing model metadata".to_string()))?;
    config.reasoning_effort = config
        .reasoning_effort
        .or_else(|| model_info.default_reasoning_level.clone())
        .map(|effort| model_info.resolve_reasoning_effort(effort));
    Ok(config)
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

pub(in crate::agent) async fn fresh_child_history_and_tools(
    parent: &CodexThread,
    config: &Config,
    version: MultiAgentVersion,
) -> (Option<ThreadHistoryMode>, Vec<DynamicToolSpec>) {
    // Legacy is explicit too; None now defaults to Paginated.
    let history_mode = Some(parent.config_snapshot().await.history_mode);
    (
        history_mode,
        inherited_dynamic_tools(parent, config, version).await,
    )
}

pub(in crate::agent) async fn inherited_dynamic_tools(
    parent: &CodexThread,
    config: &Config,
    version: MultiAgentVersion,
) -> Vec<DynamicToolSpec> {
    if version == MultiAgentVersion::V2
        && config.features.enabled(Feature::MultiAgentV2DynamicTools)
    {
        parent.session.dynamic_tools().await.to_vec()
    } else {
        Vec::new()
    }
}

impl LocalAgentControl {
    /// Spawn a new agent thread and submit the initial prompt.
    #[cfg(test)]
    pub(crate) async fn spawn_agent(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
    ) -> CodexResult<ThreadId> {
        let (spawned_agent, _) = Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            SpawnAgentOptions::default(),
        ))
        .await?;
        Ok(spawned_agent.thread_id)
    }

    pub(super) async fn spawn_agent_internal(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<(LiveAgent, ThreadConfigSnapshot)> {
        let membership = self.runtime.admit_start()?;
        let spawn_started_at = Instant::now();
        let state = self.runtime.upgrade()?;
        // Resolve the parent before inheriting settings or copying attachments.
        // Recovering it later from history only repairs the child's metadata.
        let parent_thread_id = session_source
            .as_ref()
            .and_then(SessionSource::parent_thread_id)
            .or(options.parent_thread_id);
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &InitialHistory::New,
                session_source.as_ref(),
                parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        let product_sku = config.apps_mcp_product_sku.clone();
        if let Some(session_source) = session_source.as_ref() {
            self.ensure_execution_capacity(multi_agent_version, session_source)?;
        }
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let spawn_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && session_source
                .as_ref()
                .is_some_and(is_v2_resident_session_source);
        let (residency_slot, residency_reservation) = if spawn_uses_v2_residency {
            let residency_reservation_started_at = Instant::now();
            let residency_slot = self
                .reserve_v2_residency_slot(
                    &state,
                    &config,
                    &membership,
                    /*protected_thread_id*/ None,
                )
                .await?;
            (
                Some(residency_slot),
                Some(residency_reservation_started_at.elapsed()),
            )
        } else {
            (None, None)
        };
        let reservation_max_threads = if spawn_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = self
            .runtime
            .registry
            .reserve_spawn_slot(reservation_max_threads)?;
        let inherited_environments = match &options.environments {
            Some(environments) => Some(environments.clone()),
            None => {
                self.inherited_environments_for_source(&state, session_source.as_ref())
                    .await
            }
        };
        // Omitted plugin choices follow the parent even when no history is copied.
        // Explicit choices, including an empty list, belong to the spawning turn.
        let mut disabled_plugin_ids = options.disabled_plugin_ids.clone();
        if disabled_plugin_ids.is_none()
            && let Some(parent_thread_id) = parent_thread_id
            && let Ok(parent_thread) = state.get_thread(parent_thread_id).await
        {
            disabled_plugin_ids = Some(
                parent_thread
                    .thread_settings_snapshot()
                    .await
                    .disabled_plugin_ids,
            );
        }
        let inherited_exec_policy =
            inherited_exec_policy_for_source(&state, session_source.as_ref(), &config).await;
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

        // Capture child defaults once. Copying conversation history must not change
        // the capabilities of the turn which requested the spawn.
        let mut start_options = StartThreadOptions {
            session_source: session_source.clone(),
            thread_source: session_source.as_ref().map(|_| ThreadSource::Subagent),
            environments: options
                .environments
                .as_ref()
                .map(TurnEnvironmentSnapshot::inheritable_requests),
            inherited_environments,
            disabled_plugin_ids,
            ..StartThreadOptions::new(config.clone())
        };
        if options.environments.is_none()
            && let Some(parent_thread_id) = parent_thread_id
        {
            let parent = state.get_thread(parent_thread_id).await?;
            let roots = combine_selected_capability_roots(
                &parent.session.services.selected_capability_roots,
                start_options
                    .inherited_environments
                    .as_ref()
                    .map(TurnEnvironmentSnapshot::selected_capability_roots),
            );
            start_options.thread_extension_init.insert(roots);
        }
        let fork_context = match (session_source, options.fork_mode.as_ref()) {
            (Some(session_source), Some(_)) => {
                let fork_context_started_at = Instant::now();
                let (initial_history, history_mode) = prepare_forked_history(
                    &state,
                    &session_source,
                    &options,
                    &config,
                    multi_agent_version,
                )
                .await
                .map_err(|err| err.with_agent_context(AgentErrorContext::ForkHistory))?;
                start_options.initial_history = initial_history;
                start_options.history_mode = history_mode;
                Some(fork_context_started_at.elapsed())
            }
            (Some(_), None) => {
                if let Some(parent_thread_id) = parent_thread_id
                    && let Ok(parent_thread) = state.get_thread(parent_thread_id).await
                {
                    (start_options.history_mode, start_options.dynamic_tools) =
                        fresh_child_history_and_tools(&parent_thread, &config, multi_agent_version)
                            .await;
                }
                None
            }
            (None, _) => None,
        };
        // Fork startup can persist copied history before returning a live child.
        // Publish its deletion relationship before any child storage is created.
        let child_thread_id = self.runtime.generate_thread_id();
        if state.threads.read().await.contains_key(&child_thread_id) {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {child_thread_id} is already running",
            ))
            .with_agent_context(AgentErrorContext::ChildStartup));
        }
        start_options.reserved_thread_id = Some(child_thread_id);
        let mut pending_spawn = PendingSpawn::new(
            Arc::clone(&state),
            child_thread_id,
            membership,
            SpawnOwner::Local,
        );
        let edge_write_started_at = Instant::now();
        let control = self.clone();
        let source = notification_source.clone();
        let ephemeral = config.ephemeral;
        pending_spawn.set_edge_write(tokio::spawn(async move {
            control
                .persist_thread_spawn_edge_for_source(ephemeral, child_thread_id, source.as_ref())
                .await
        }));
        pending_spawn.wait_for_edge().await?;
        let edge_write = edge_write_started_at.elapsed();
        let child_create_started_at = Instant::now();
        let new_thread = state
            .spawn_child_thread(
                start_options,
                self.clone(),
                parent_thread_id,
                inherited_exec_policy,
            )
            .await
            .map_err(|err| err.with_agent_context(AgentErrorContext::ChildStartup))?;
        let child_create = child_create_started_at.elapsed();
        agent_metadata.agent_id = Some(new_thread.thread_id);
        if let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = notification_source.as_ref()
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
            new_thread
                .thread
                .emit_subagent_session_started(client_metadata, /*resumed_created_at*/ None)
                .await;
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

        let durability_wait_started_at = Instant::now();
        // Ordinary spawned children also need discoverable rollout metadata before
        // publication. Their spawn edge was already persisted before child startup.
        if options.fork_mode.is_some()
            || matches!(
                notification_source.as_ref(),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
            )
        {
            new_thread.thread.ensure_rollout_materialized().await;
        }
        let durability_wait = edge_write + durability_wait_started_at.elapsed();

        // Capture startup settings before the child can publish metadata for a later turn.
        let spawn_config = if multi_agent_version == MultiAgentVersion::V2 {
            Some(effective_spawn_config(&new_thread.thread).await?)
        } else {
            None
        };

        let start_options = TurnStartOptions {
            parent_turn_id: options.parent_turn_id,
            turn_trigger: options.turn_trigger,
            root_turn_id: options.root_turn_id,
            cyber_access_program: options.cyber_access_program,
            ..Default::default()
        };
        let input_admission_started_at = Instant::now();
        match initial_input {
            SpawnInitialInput::UserInput(input) => {
                self.send_input(new_thread.thread_id, input, start_options)
                    .await
                    .map_err(|err| err.with_agent_context(AgentErrorContext::InputAdmission))?;
            }
            SpawnInitialInput::InterAgentCommunication(communication, context) => {
                self.send_inter_agent_communication_after_capacity_check(
                    new_thread.thread_id,
                    &state,
                    CapturedRecipient::Loaded(Arc::clone(&new_thread.thread)),
                    communication,
                    context,
                    start_options,
                )
                .await
                .map_err(|err| err.with_agent_context(AgentErrorContext::InputAdmission))?;
            }
        }
        let input_admission = input_admission_started_at.elapsed();
        // Keep provisional ownership through the final awaits. Commit and return to the
        // backend caller in one poll; later tool-result publication is outside this boundary.
        let status = self.get_status(new_thread.thread_id).await;
        let config = match spawn_config {
            Some(config) => config,
            None => new_thread.thread.config_snapshot().await,
        };
        reservation.commit(agent_metadata.clone());
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(new_thread.thread_id);
        }
        let _membership = pending_spawn.disarm();

        // Notify a new thread has been created. This notification will be processed by clients
        // to subscribe or drain this newly created thread.
        // TODO(jif) add helper for drain
        state.notify_thread_created(new_thread.thread_id);

        let agent = LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status,
        };
        let session_telemetry = new_thread
            .thread
            .session_telemetry()
            .with_product_sku(product_sku.as_deref());
        record_spawn_success(
            &session_telemetry,
            options.fork_mode.as_ref(),
            multi_agent_version,
            SpawnMeasurements {
                history_mode: config.history_mode,
                residency_reservation,
                fork_context,
                child_create,
                durability_wait,
                input_admission,
                total: spawn_started_at.elapsed(),
            },
        );
        Ok((agent, config))
    }
}
