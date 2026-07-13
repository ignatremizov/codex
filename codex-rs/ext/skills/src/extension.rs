use std::sync::Arc;

use codex_core_skills::HostSkillsSnapshot;
use codex_core_skills::SkillInstructions;
use codex_core_skills::default_skill_metadata_budget;
use codex_core_skills::injection::HostSkillsCatalogInWorldState;
use codex_core_skills::injection::InjectedHostSkillPrompts;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::GoalSkillActivations;
use codex_extension_api::PromptFragment;
use codex_extension_api::SkillInvocationContributor;
use codex_extension_api::SkillInvocationInput;
use codex_extension_api::SkillInvocationKind;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContribution;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_mcp::McpResourceClient;
use codex_otel::MetricsClient;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;

use crate::SkillsExtensionConfig;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;
use crate::provider::HostSkillProvider;
use crate::provider::SkillListQuery;
use crate::provider::SkillReadRequest;
use crate::render::available_skills_fragment;
use crate::render::truncate_main_prompt_contents;
use crate::selection::collect_explicit_skill_mentions;
use crate::shadow_selection_experiment::ShadowSelectionExperiment;
use crate::sources::SkillProviders;
use crate::state::ExecutorSkillsStepState;
use crate::state::SkillsThreadState;
use crate::tools::skill_tools;
use crate::world_state::executor_skills_world_state_section;
use crate::world_state::host_skills_world_state_section;

struct SkillsExtension<C> {
    providers: SkillProviders,
    event_sink: Arc<dyn ExtensionEventSink>,
    config_from_host: Arc<dyn Fn(&C) -> SkillsExtensionConfig + Send + Sync>,
    shadow_selection: Arc<ShadowSelectionExperiment>,
}

impl<C> ThreadLifecycleContributor<C> for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let orchestrator_skills_available = !input
                .environments
                .iter()
                .any(|environment| environment.environment_id == LOCAL_ENVIRONMENT_ID);
            let thread_state = SkillsThreadState::new(
                (self.config_from_host)(input.config),
                orchestrator_skills_available,
            );
            if let Some(history) = input
                .thread_store
                .get::<codex_extension_api::ConversationHistory>()
            {
                thread_state.restore_promoted_skills(&history);
            }
            input.thread_store.insert(thread_state);
            input
                .thread_store
                .get_or_init::<GoalSkillActivations>(GoalSkillActivations::default);
        })
    }
}

impl<C> ConfigContributor<C> for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        _previous_config: &C,
        new_config: &C,
    ) {
        let next_config = (self.config_from_host)(new_config);
        if let Some(state) = thread_store.get::<SkillsThreadState>() {
            state.set_config(next_config);
        } else {
            let orchestrator_skills_available = true;
            thread_store.insert(SkillsThreadState::new(
                next_config,
                orchestrator_skills_available,
            ));
        }
    }
}

impl<C> ContextContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute_thread_context<'a>(
        &'a self,
        session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
                return Vec::new();
            };
            let config = thread_state.config();
            if !config.include_instructions {
                return Vec::new();
            }

            let host_snapshot = thread_state
                .host_snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let catalog = self
                .list_skills(
                    SkillListQuery {
                        turn_id: thread_store.level_id().to_string(),
                        executor_roots: Vec::new(),
                        host_snapshot: host_snapshot.clone(),
                        include_host_skills: host_snapshot.is_some(),
                        include_bundled_skills: config.bundled_skills_enabled,
                        include_orchestrator_skills: thread_state.orchestrator_skills_enabled(),
                        mcp_resources: session_store.get::<McpResourceClient>(),
                    },
                    &thread_state,
                )
                .await;
            for warning in &catalog.warnings {
                self.emit_warning(thread_store.level_id(), warning.clone());
            }
            let (promoted, unresolved) = thread_state.resolve_promoted_skills(&catalog);
            let promoted_identities = thread_state.promoted_skill_identities();
            if unresolved > 0 {
                self.emit_unresolved_promotions_warning(thread_store.level_id(), unresolved);
            }
            let include_usage = thread_store
                .get::<ModelInfo>()
                .is_some_and(|model_info| model_info.include_skills_usage_instructions);
            let fragments =
                available_skills_fragment(&catalog, &promoted, &promoted_identities, include_usage)
                    .map(|fragment| vec![PromptFragment::developer_capability(fragment.render())])
                    .unwrap_or_default();
            thread_state.acknowledge_promoted_projection(&promoted);
            fragments
        })
    }

    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let Some(thread_state) = input.thread_store.get::<SkillsThreadState>() else {
                return Vec::new();
            };
            let config = thread_state.config();
            let catalog = thread_state
                .executor_catalog_snapshot(
                    &self.providers,
                    SkillListQuery {
                        turn_id: input.turn_id.to_string(),
                        executor_roots: input.ready_selected_capability_roots.to_vec(),
                        host_snapshot: None,
                        include_host_skills: false,
                        include_bundled_skills: config.bundled_skills_enabled,
                        include_orchestrator_skills: false,
                        mcp_resources: input.session_store.get::<McpResourceClient>(),
                    },
                )
                .await;
            input
                .turn_store
                .insert(ExecutorSkillsStepState(catalog.clone()));
            let model_info = input.thread_store.get::<ModelInfo>();
            let include_usage = model_info
                .as_deref()
                .is_some_and(|model_info| model_info.include_skills_usage_instructions);
            let mut sections = vec![executor_skills_world_state_section(
                &catalog,
                config.include_instructions,
                include_usage,
            )];
            if let Some(host_snapshot) = input.turn_store.get::<HostSkillsSnapshot>()
                && self.providers.has_host_provider()
            {
                input.turn_store.insert(HostSkillsCatalogInWorldState);
                sections.push(host_skills_world_state_section(
                    &host_snapshot,
                    config.include_instructions,
                    include_usage,
                    default_skill_metadata_budget(
                        model_info
                            .as_deref()
                            .and_then(|model_info| model_info.context_window),
                    ),
                ));
            }
            sections
        })
    }
}

impl<C> ToolContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn tools(
        &self,
        session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
            return Vec::new();
        };
        if !self.providers.has_orchestrator_provider()
            || !thread_state.orchestrator_skills_enabled()
        {
            return Vec::new();
        }

        skill_tools(
            self.providers.clone(),
            session_store.get::<McpResourceClient>(),
            thread_state,
            Arc::clone(&self.shadow_selection),
        )
    }
}

impl<C> SkillInvocationContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_skill_invocation<'a>(
        &'a self,
        input: SkillInvocationInput<'a>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            match input.kind {
                SkillInvocationKind::Implicit => {
                    if let Some(state) = input
                        .thread_store
                        .get::<SkillsThreadState>()
                        .and_then(|state| state.shadow_selection_turn(input.turn_id))
                    {
                        self.shadow_selection
                            .record_invocation(&state, input.skill_resource);
                    }
                }
                SkillInvocationKind::Explicit => {}
            }
        })
    }
}

impl<C> TurnInputContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext,
        session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let contribution = self
                .contribute_durable(input, session_store, thread_store, turn_store)
                .await;
            let (fragments, acknowledgement) = contribution.into_parts();
            if let Some(acknowledgement) = acknowledgement {
                acknowledgement.acknowledge();
            }
            fragments
        })
    }

    fn contribute_durable<'a>(
        &'a self,
        input: TurnInputContext,
        session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, TurnInputContribution> {
        Box::pin(async move {
            let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
                return TurnInputContribution::default();
            };

            let config = thread_state.config();
            let host_snapshot = turn_store.get::<HostSkillsSnapshot>();
            let host_catalog_in_world_state =
                turn_store.get::<HostSkillsCatalogInWorldState>().is_some();
            if let Some(host_snapshot) = host_snapshot.as_ref() {
                *thread_state
                    .host_snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Arc::clone(host_snapshot));
            }
            let query = SkillListQuery {
                turn_id: input.turn_id.clone(),
                executor_roots: Vec::new(),
                host_snapshot: host_snapshot.clone(),
                include_host_skills: !host_catalog_in_world_state,
                include_bundled_skills: config.bundled_skills_enabled,
                include_orchestrator_skills: thread_state.orchestrator_skills_enabled(),
                mcp_resources: session_store.get::<McpResourceClient>(),
            };
            let host_query = query.clone();
            let mut catalog = turn_store
                .get::<ExecutorSkillsStepState>()
                .map(|executor_skills| executor_skills.0.clone())
                .unwrap_or_default();
            catalog.extend(self.list_skills(query, &thread_state).await);
            for warning in &catalog.warnings {
                self.emit_warning(&input.turn_id, warning.clone());
            }

            let mut selected_entries = collect_explicit_skill_mentions(&input.user_input, &catalog);
            let goal_selections = thread_store
                .get::<GoalSkillActivations>()
                .map(|activations| activations.snapshot())
                .unwrap_or_default();
            let goal_mentions = goal_selections
                .iter()
                .map(|selection| codex_protocol::user_input::UserInput::Mention {
                    name: selection.name.clone(),
                    path: selection.path.clone(),
                })
                .collect::<Vec<_>>();
            let goal_entries = collect_explicit_skill_mentions(&goal_mentions, &catalog);
            let unresolved_goal_selections =
                goal_selections.len().saturating_sub(goal_entries.len());
            for entry in goal_entries {
                if !selected_entries.iter().any(|candidate| {
                    candidate.authority == entry.authority && candidate.id == entry.id
                }) {
                    selected_entries.push(entry);
                }
            }
            if unresolved_goal_selections > 0 {
                self.emit_warning(
                    &input.turn_id,
                    format!(
                        "{unresolved_goal_selections} goal-selected skill(s) could not be resolved \
                         and were omitted from the skills inventory."
                    ),
                );
            }
            let (resolved_promoted, unresolved) = thread_state.resolve_promoted_skills(&catalog);
            if unresolved > 0 {
                self.emit_unresolved_promotions_warning(&input.turn_id, unresolved);
            }
            let promotable_entries = selected_entries
                .iter()
                .filter(|entry| {
                    !entry.prompt_visible
                        && matches!(
                            entry.authority.kind,
                            SkillSourceKind::Host | SkillSourceKind::Orchestrator
                        )
                })
                .cloned()
                .collect::<Vec<_>>();
            let non_promotable_count = selected_entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.authority.kind,
                        SkillSourceKind::Executor | SkillSourceKind::Custom(_)
                    )
                })
                .count();
            if non_promotable_count > 0 {
                self.emit_warning(
                    &input.turn_id,
                    format!(
                        "{non_promotable_count} executor/custom skill(s) were kept turn-local \
                         because their providers expose no durable model read route."
                    ),
                );
            }
            let (next_promoted, changed, omitted_promotions) =
                thread_state.promoted_with(&promotable_entries);
            let mut promoted_entries = merge_skill_entries(resolved_promoted, &promotable_entries);
            promoted_entries.retain(|entry| {
                next_promoted
                    .iter()
                    .any(|identity| identity.matches_entry(entry))
            });
            let projection_changed = thread_state.promoted_projection_changed(&promoted_entries);
            if omitted_promotions > 0 {
                self.emit_warning(
                    &input.turn_id,
                    format!(
                        "{omitted_promotions} skill promotion(s) were omitted because the bounded \
                         promoted inventory is full."
                    ),
                );
            }

            let shadow_selection_turn = if config.shadow_selection_enabled {
                let mut shadow_catalog = catalog.clone();
                if host_catalog_in_world_state && host_snapshot.is_some() {
                    shadow_catalog.extend(self.providers.list_host_for_turn(host_query).await);
                }
                Some(
                    self.shadow_selection
                        .run(&input.user_input, &shadow_catalog),
                )
            } else {
                None
            };
            thread_state
                .replace_shadow_selection_turn(input.turn_id.clone(), shadow_selection_turn);

            let mut fragments: Vec<Box<dyn ContextualUserFragment + Send>> = Vec::new();
            if (changed || projection_changed) && config.include_instructions {
                let include_usage = thread_store
                    .get::<ModelInfo>()
                    .is_some_and(|model_info| model_info.include_skills_usage_instructions);
                if let Some(fragment) = available_skills_fragment(
                    &catalog,
                    &promoted_entries,
                    &next_promoted,
                    include_usage,
                ) {
                    fragments.push(Box::new(fragment));
                }
            }

            for entry in selected_entries.iter().filter(|entry| {
                matches!(
                    entry.authority.kind,
                    SkillSourceKind::Executor | SkillSourceKind::Custom(_)
                )
            }) {
                match thread_state
                    .read_skill(
                        &self.providers,
                        SkillReadRequest {
                            authority: entry.authority.clone(),
                            package: entry.id.clone(),
                            resource: entry.main_prompt.clone(),
                            host_snapshot: host_snapshot.clone(),
                            mcp_resources: session_store.get::<McpResourceClient>(),
                        },
                    )
                    .await
                {
                    Ok(read_result) => {
                        let (contents, truncated) =
                            truncate_main_prompt_contents(&read_result.contents);
                        if truncated {
                            let name = &entry.name;
                            let warning = format!(
                                "Skill `{name}` exceeded the turn-local prompt limit and was \
                                 truncated."
                            );
                            self.emit_warning(&input.turn_id, warning);
                        }
                        fragments.push(Box::new(SkillInstructions::new(
                            entry.name.clone(),
                            entry.rendered_path(),
                            contents,
                        )));
                    }
                    Err(err) => {
                        let name = &entry.name;
                        let message = err.message;
                        let warning = format!("Failed to read skill `{name}`: {message}");
                        self.emit_warning(&input.turn_id, warning);
                    }
                }
            }

            if (changed || projection_changed) && !fragments.is_empty() {
                TurnInputContribution::with_acknowledgement(fragments, move || {
                    thread_state.acknowledge_promoted_skills(next_promoted, &promoted_entries);
                })
            } else {
                TurnInputContribution::new(fragments)
            }
        })
    }
}

impl<C> SkillsExtension<C> {
    #[tracing::instrument(level = "trace", skip_all)]
    async fn list_skills(
        &self,
        mut query: SkillListQuery,
        thread_state: &SkillsThreadState,
    ) -> SkillCatalog {
        let include_orchestrator_skills = query.include_orchestrator_skills;
        let orchestrator_query = query.clone();
        let mcp_resources = orchestrator_query.mcp_resources.clone();
        query.include_orchestrator_skills = false;

        let mut catalog = self.providers.list_for_turn(query).await;
        if include_orchestrator_skills {
            let orchestrator_catalog = thread_state
                .orchestrator_catalog_snapshot(
                    mcp_resources.as_deref(),
                    self.providers
                        .list_orchestrator_for_turn(orchestrator_query),
                )
                .await;
            catalog.extend(orchestrator_catalog);
        }
        catalog
    }

    fn emit_warning(&self, turn_id: &str, message: String) {
        self.event_sink.emit(Event {
            id: turn_id.to_string(),
            msg: EventMsg::Warning(WarningEvent { message }),
        });
    }

    fn emit_unresolved_promotions_warning(&self, turn_id: &str, unresolved: usize) {
        let skill_word = if unresolved == 1 { "skill" } else { "skills" };
        self.emit_warning(
            turn_id,
            format!(
                "{unresolved} promoted {skill_word} could not be resolved; omitted from the \
                 current skills inventory."
            ),
        );
    }
}

fn merge_skill_entries(
    mut existing: Vec<SkillCatalogEntry>,
    selected: &[SkillCatalogEntry],
) -> Vec<SkillCatalogEntry> {
    for entry in selected {
        if !existing
            .iter()
            .any(|candidate| candidate.authority == entry.authority && candidate.id == entry.id)
        {
            existing.push(entry.clone());
        }
    }
    existing
}

pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    config_from_host: impl Fn(&C) -> SkillsExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    install_with_providers(
        registry,
        SkillProviders::new().with_host_provider(Arc::new(HostSkillProvider::new())),
        config_from_host,
    );
}

pub fn install_with_providers<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    providers: SkillProviders,
    config_from_host: impl Fn(&C) -> SkillsExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    install_with_providers_and_metrics(
        registry,
        providers,
        /*metrics_client*/ None,
        config_from_host,
    );
}

pub fn install_with_providers_and_metrics<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    providers: SkillProviders,
    metrics_client: Option<MetricsClient>,
    config_from_host: impl Fn(&C) -> SkillsExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(SkillsExtension {
        providers,
        event_sink: registry.event_sink(),
        config_from_host: Arc::new(config_from_host),
        shadow_selection: Arc::new(ShadowSelectionExperiment::new(metrics_client)),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.prompt_contributor(extension.clone());
    registry.turn_input_contributor(extension.clone());
    registry.skill_invocation_contributor(extension.clone());
    registry.tool_contributor(extension);
}
