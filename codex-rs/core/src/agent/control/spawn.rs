use super::residency::is_v2_resident_session_source;
use super::spawn_guard::PendingSpawn;
use super::spawn_telemetry::SpawnMeasurements;
use super::spawn_telemetry::record_spawn_success;
use super::*;
use crate::agent::types::AgentMetadata;
use crate::agent::types::LiveAgent;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::context::ContextualUserFragment;
use crate::context::CurrentTimeReminder;
use crate::context::CurrentTimeUnavailable;
use crate::context::DeveloperInstructions;
use crate::context::ManagedDeveloperInstructions;
use crate::context::MultiAgentModeInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::world_state::PersistentModeState;
use crate::session::multi_agents::resolve_usage_hints;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_extension_api::ExtensionDataInit;
use codex_history::ResponseItemEnvelope;
use codex_history::rollout_without_exact_rollback_ranges;
use codex_prompts::ResolvedModelMessages;
use codex_thread_store::PersistContext;
use std::time::Duration;
use std::time::Instant;

const AGENT_NAMES: &str = include_str!("../../../assets/agent/agent_names.txt");

struct SpawnAgentThreadInheritance {
    environments: Option<TurnEnvironmentSnapshot>,
    exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
}

struct SpawnedThreadResult {
    new_thread: crate::thread_manager::NewThread,
    fork_context: Option<Duration>,
    child_create: Duration,
}

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

fn keep_forked_rollout_item(item: &RolloutItem, preserve_context_baselines: bool) -> bool {
    match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::Message { role, phase, .. } => match role.as_str() {
                "system" | "developer" | "user" => true,
                "assistant" => *phase == Some(MessagePhase::FinalAnswer),
                _ => false,
            },
            ResponseItem::FunctionCallOutput { call_id: None, .. }
            | ResponseItem::ConfigurationUpdate { .. } => true,
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
        | RolloutItem::RetainedContext(_)
        | RolloutItem::SecurityRiskScore(_) => false,
        // Full-history forks preserve the cached prompt prefix and can keep diffing
        // from the parent's durable baseline. Truncated forks drop part of that prompt,
        // so they must rebuild context on their first child turn.
        RolloutItem::TurnContext(_) | RolloutItem::WorldState(_) => preserve_context_baselines,
        // Child threads inherit model context, not the parent's cumulative usage state.
        RolloutItem::TokenUsageRecord(_) => false,
        RolloutItem::Compacted(_) | RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => true,
    }
}

fn retain_forked_developer_message(item: &mut ResponseItem, usage_hint_texts: &[String]) -> bool {
    if !matches!(item, ResponseItem::Message { role, .. } if role == "developer") {
        return true;
    }

    let Some(mut content) = to_annotated_content(item) else {
        return false;
    };
    content.retain(|content_item| {
        if content_item.kind().0 == "guardian.approved_action" {
            return false;
        }
        let ContentItem::InputText { text } = content_item.content() else {
            return true;
        };

        !(MultiAgentRoleInstructions::matches_text(text)
            || text
                .starts_with(crate::guardian::AUTO_REVIEW_DENIED_ACTION_APPROVAL_DEVELOPER_PREFIX)
            || MultiAgentModeInstructions::matches_text(text)
            || CurrentTimeReminder::matches_text(text)
            || CurrentTimeUnavailable::matches_text(text)
            || usage_hint_texts
                .iter()
                .any(|usage_hint_text| usage_hint_text == text))
    });
    !content.is_empty() && set_annotated_content(item, content).is_some()
}

pub(super) async fn load_agent_model_context(
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
        let control = self.clone();
        tokio::spawn(async move {
            Box::pin(control.spawn_agent_owned(config, initial_input, session_source, options))
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("agent spawn worker failed: {error}")))?
    }

    async fn spawn_agent_owned(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<(LiveAgent, ThreadConfigSnapshot)> {
        let spawn_started_at = Instant::now();
        let state = self.runtime.upgrade()?;
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &InitialHistory::New,
                session_source.as_ref(),
                options.parent_thread_id,
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
                .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
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
        // A same-UUID resumed parent is not the parent that requested this spawn.
        // Capture once and use that exact instance for both native completion and `w`.
        let completion_parent = match session_source.as_ref().and_then(SessionSource::parent_thread_id) {
            Some(parent_id) => Some(state.get_thread(parent_id).await?),
            None => None,
        };
        let inheritance = SpawnAgentThreadInheritance {
            environments: match &options.environments {
                Some(environments) => Some(environments.clone()),
                None => {
                    self.inherited_environments_for_source(&state, session_source.as_ref())
                        .await
                }
            },
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

        // The same `LocalAgentControl` is sent to spawn the thread.
        let SpawnedThreadResult {
            new_thread,
            fork_context,
            child_create,
        } = match (session_source, options.fork_mode.as_ref(), inheritance) {
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
                    // Legacy is an explicit selection too; None now defaults to Paginated.
                    Some(parent_thread.config_snapshot().await.history_mode)
                } else {
                    None
                };
                let environments = options
                    .environments
                    .as_ref()
                    .map(TurnEnvironmentSnapshot::inheritable_selections);
                let child_create_started_at = Instant::now();
                let new_thread = Box::pin(state.spawn_new_thread_with_source(
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
                    environments,
                ))
                .await?;
                SpawnedThreadResult {
                    new_thread,
                    fork_context: None,
                    child_create: child_create_started_at.elapsed(),
                }
            }
            (None, _, _) => {
                let child_create_started_at = Instant::now();
                let new_thread =
                    Box::pin(state.spawn_new_thread(config.clone(), self.clone())).await?;
                SpawnedThreadResult {
                    new_thread,
                    fork_context: None,
                    child_create: child_create_started_at.elapsed(),
                }
            }
        };
        agent_metadata.agent_id = Some(new_thread.thread_id);
        let mut pending_spawn = PendingSpawn::new(Arc::clone(&state), new_thread.thread_id);

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
        if let (Some(parent), Some(source)) = (&completion_parent, &notification_source) {
            self.bind_completion_watcher_with_parent(
                &new_thread.thread,
                parent,
                source.clone(),
                child_reference,
                agent_metadata.agent_path.clone(),
                new_thread.thread.multi_agent_version().unwrap_or(MultiAgentVersion::V1),
            )?;
        }


        let control = self.clone();
        let child = Arc::clone(&new_thread.thread);
        let child_thread_id = new_thread.thread_id;
        let source = notification_source.clone();
        pending_spawn.set_edge_write(tokio::spawn(async move {
            control
                .persist_thread_spawn_edge_for_source(
                    child.as_ref(),
                    child_thread_id,
                    source.as_ref(),
                )
                .await;
        }));
        let durability_wait_started_at = Instant::now();
        // Ordinary spawned children also need discoverable rollout metadata before
        // publication. Retain the provisional guard and its independently joined edge write.
        if options.fork_mode.is_some()
            || matches!(
                notification_source.as_ref(),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
            )
        {
            tokio::join!(
                new_thread
                    .thread
                    .session
                    .ensure_rollout_materialized(PersistContext::Standard),
                pending_spawn.wait_for_edge(),
            );
        } else {
            pending_spawn.wait_for_edge().await;
        }
        let durability_wait = durability_wait_started_at.elapsed();

        let start_options = TurnStartOptions {
            parent_turn_id: options.parent_turn_id,
            turn_trigger: options.turn_trigger,
            root_turn_id: options.root_turn_id,
            cyber_access_program: options.cyber_access_program,
            ..Default::default()
        };
        let input_admission_started_at = Instant::now();
        let submission = match initial_input {
            SpawnInitialInput::UserInput(input) => {
                if let Some(observer) = &completion_parent {
                    self.send_input_observing_response(
                        new_thread.thread_id,
                        input,
                        start_options,
                        observer.session.presentation_id(),
                        options.response_observation,
                    ).await
                } else {
                    self.send_input(new_thread.thread_id, input, start_options)
                        .await
                }
            }
            SpawnInitialInput::InterAgentCommunication(communication, context) => {
                self.send_inter_agent_communication_after_capacity_check(
                    new_thread.thread_id,
                    &state,
                    &new_thread.thread,
                    communication,
                    context,
                    start_options,
                )
                .await
            }
        };
        // PendingSpawn already owns teardown, discard, and the ordered spawn-edge close
        // until input acceptance. Do not start a second competing close transaction.
        submission?;
        let input_admission = input_admission_started_at.elapsed();
        reservation.commit(agent_metadata.clone());
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(new_thread.thread_id);
        }
        pending_spawn.disarm();

        // Notify a new thread has been created. This notification will be processed by clients
        // to subscribe or drain this newly created thread.
        // TODO(jif) add helper for drain
        state.notify_thread_created(new_thread.thread_id);

        let agent = LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status: self.get_status(new_thread.thread_id).await,
        };
        let config = new_thread.thread.config_snapshot().await;
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

    async fn spawn_forked_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        config: Config,
        session_source: SessionSource,
        options: &SpawnAgentOptions,
        inheritance: SpawnAgentThreadInheritance,
        multi_agent_version: MultiAgentVersion,
    ) -> CodexResult<SpawnedThreadResult> {
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

        let fork_context_started_at = Instant::now();
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
                    ResolvedModelMessages::bundled().multi_agent(),
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
        let mut preserve_context_baselines = matches!(fork_mode, SpawnAgentForkMode::FullHistory);
        if preserve_context_baselines {
            for item in forked_rollout_items.iter().rev() {
                let RolloutItem::Compacted(compacted) = item else {
                    continue;
                };
                // Legacy checkpoints force the child to rebuild context regardless of the
                // live parent's reference baseline; an older superseded checkpoint does not.
                if compacted.replacement_history.is_none() {
                    preserve_context_baselines = false;
                }
                break;
            }
        }
        let mut replaced_parent_developer_instructions = false;
        // Scrub inherited hints and replace only the parent's developer-instruction fragment.
        // Compaction stores response items separately, so sanitize both top-level messages and
        // compacted replacement histories with the same policy.
        let retain_forked_item = |envelope: &mut ResponseItemEnvelope, replaced: &mut bool| {
            if !super::fork_goal_context::retain_without_goal_context(envelope) {
                return false;
            }
            if !super::fork_notification_context::retain_without_notification_context(envelope) {
                return false;
            }
            if multi_agent_version == MultiAgentVersion::V2
                && matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user" || role == "assistant")
            {
                // Persist the scope of every inherited conversational message, including the suffix
                // after a checkpoint. Resume must not recapture it as local authorization.
                envelope
                    .metadata
                    .get_or_insert_default()
                    .inherited_user_message = true;
            }
            if let Some(metadata) = &mut envelope.metadata
                && (metadata.sender_user_messages.take().is_some()
                    || !matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user"))
            {
                // Assistant and tool positions belong to the parent counter, not the child.
                metadata.user_input_order = None;
            }
            let response_item = &mut envelope.item;
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
                        return preserve_context_baselines;
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
                    let replacement = if preserve_context_baselines {
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
            if !keep_forked_rollout_item(item, preserve_context_baselines)
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
                    // This compaction becomes part of the subagent's initial history. Rewrite its
                    // metadata to describe the child rather than the parent.
                    compacted.latest_token_usage_record = None;
                    if let Some(resume_metadata) = &mut compacted.resume_metadata {
                        resume_metadata.multi_agent_version = Some(multi_agent_version);
                        if !preserve_context_baselines {
                            resume_metadata.previous_turn_settings = None;
                        }
                    }
                    // Parent-local review evidence must not become the child's authorization.
                    // Root user authorization is collected separately by the host.
                    compacted.guardian_history = None;
                    // Only V2 fetches root authorization live. Its local scope starts known-empty;
                    // V1 must remain incomplete when inherited authorization has been stripped.
                    compacted.retained_context = (multi_agent_version == MultiAgentVersion::V2)
                        .then(codex_history::RetainedContext::default);
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
                RolloutItem::RetainedContext(_)
                | RolloutItem::AgentResponseObservation(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::SecurityRiskScore(_) => false,
            }
        });
        // Full forks reuse the parent's reference context instead of rebuilding it. If that
        // context omitted the parent's developer fragment, append the child's override so its
        // instructions still reach the model exactly once.
        if let Some(subagent_developer_instructions) = subagent_developer_instructions.as_ref()
            && preserve_context_baselines
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
        if preserve_context_baselines
            && multi_agent_version == MultiAgentVersion::V2
            && let Some(subagent_usage_hint) = options
                .multi_agent_v2_usage_hints
                .as_ref()
                .map(|hints| hints.subagent.clone())
                .unwrap_or_else(|| {
                    resolve_usage_hints(
                        &config.multi_agent_v2,
                        ResolvedModelMessages::bundled().multi_agent(),
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

        let fork_context = fork_context_started_at.elapsed();
        let child_create_started_at = Instant::now();
        let new_thread = state
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
                /*environments*/ None,
                thread_extension_init,
            )
            .await?;
        let child_create = child_create_started_at.elapsed();
        Ok(SpawnedThreadResult {
            new_thread,
            fork_context: Some(fork_context),
            child_create,
        })
    }
}
