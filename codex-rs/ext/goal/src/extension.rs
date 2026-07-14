use std::sync::Arc;
use std::sync::Weak;

use codex_analytics::AnalyticsEventsClient;
use codex_core::ThreadManager;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::GoalSkillActivations;
use codex_extension_api::PostCompactionContextContribution;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadResumeInput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::TokenUsageContributor;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_otel::MetricsClient;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::TokenUsageInfo;

use crate::accounting::BudgetLimitedGoalDisposition;
use crate::accounting::GoalAccountingState;
use crate::analytics::GoalAnalytics;
use crate::api::GoalService;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::runtime::ActiveGoalStopReason;
use crate::runtime::GoalRuntimeConfig;
use crate::runtime::GoalRuntimeHandle;
use crate::runtime::InactiveGoalHistory;
use crate::spec::UPDATE_GOAL_TOOL_NAME;
use crate::steering::budget_limit_steering_item;
use crate::steering::continuation_steering_item;
use crate::tool::GoalToolExecutor;

#[derive(Clone, Debug)]
pub struct GoalExtensionConfig {
    pub enabled: bool,
}

impl GoalExtensionConfig {
    fn from_enabled(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[derive(Clone)]
pub struct GoalExtension<C> {
    state_dbs: Arc<codex_state::StateRuntime>,
    analytics: GoalAnalytics,
    event_emitter: GoalEventEmitter,
    metrics: GoalMetrics,
    thread_manager: Weak<ThreadManager>,
    goal_service: Arc<GoalService>,
    goals_enabled: Arc<dyn Fn(&C) -> bool + Send + Sync>,
}

impl<C> std::fmt::Debug for GoalExtension<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalExtension").finish_non_exhaustive()
    }
}

impl<C> GoalExtension<C> {
    pub(crate) fn new_with_host_capabilities(
        state_dbs: Arc<codex_state::StateRuntime>,
        analytics_events_client: AnalyticsEventsClient,
        event_sink: Arc<dyn ExtensionEventSink>,
        metrics_client: Option<MetricsClient>,
        thread_manager: Weak<ThreadManager>,
        goal_service: Arc<GoalService>,
        goals_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            state_dbs,
            analytics: GoalAnalytics::new(analytics_events_client),
            event_emitter: GoalEventEmitter::new(event_sink),
            metrics: GoalMetrics::new(metrics_client),
            thread_manager,
            goal_service,
            goals_enabled: Arc::new(goals_enabled),
        }
    }
}

impl<C> ThreadLifecycleContributor<C> for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let enabled = (self.goals_enabled)(input.config);
            let tools_available_for_thread = input.persistent_thread_state_available
                && !matches!(
                    input.session_source,
                    SessionSource::SubAgent(SubAgentSource::Review)
                );
            input
                .thread_store
                .insert(GoalExtensionConfig::from_enabled(enabled));
            let accounting_state = input
                .thread_store
                .get_or_init::<GoalAccountingState>(GoalAccountingState::default);
            let goal_skill_activations = input
                .thread_store
                .get_or_init::<GoalSkillActivations>(GoalSkillActivations::default);
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            let runtime = input.thread_store.get_or_init::<GoalRuntimeHandle>(|| {
                GoalRuntimeHandle::new(
                    thread_id,
                    Arc::clone(&self.state_dbs),
                    self.event_emitter.clone(),
                    self.metrics.clone(),
                    self.thread_manager.clone(),
                    accounting_state,
                    goal_skill_activations,
                    GoalRuntimeConfig {
                        analytics: self.analytics.clone(),
                        enabled,
                        tools_available_for_thread,
                    },
                )
            });
            runtime.set_enabled(enabled);
            self.goal_service.register_runtime(&runtime);
            if let Err(err) = runtime.restore_after_start().await {
                tracing::warn!(
                    "failed to restore goal runtime after thread start for {}: {err}",
                    runtime.thread_id()
                );
            }
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };

            if let Err(err) = runtime.restore_after_resume().await {
                tracing::warn!(
                    "failed to restore goal runtime after thread resume for {}: {err}",
                    runtime.thread_id()
                );
            }
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };

            if let Err(err) = runtime.continue_if_idle().await {
                tracing::warn!(
                    "failed to continue active goal for idle thread {}: {err}",
                    runtime.thread_id()
                );
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = goal_runtime_handle(input.thread_store) {
                self.goal_service.unregister_runtime(&runtime);
            }
        })
    }
}

impl<C> ConfigContributor<C> for GoalExtension<C>
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
        let enabled = (self.goals_enabled)(new_config);
        thread_store.insert(GoalExtensionConfig::from_enabled(enabled));
        if let Some(runtime) = goal_runtime_handle(thread_store) {
            runtime.set_enabled(enabled);
        }
    }
}

impl<C> ContextContributor for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute_post_compaction_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, PostCompactionContextContribution> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(thread_store) else {
                return PostCompactionContextContribution::default();
            };
            let Ok(goal_state_permit) = runtime.goal_state_permit().await else {
                return PostCompactionContextContribution::default();
            };
            if !runtime.is_enabled() {
                return PostCompactionContextContribution::default();
            }
            let goal = match self
                .state_dbs
                .thread_goals()
                .get_thread_goal(runtime.thread_id())
                .await
            {
                Ok(Some(goal)) if goal.status == codex_state::ThreadGoalStatus::Active => goal,
                Ok(_) => return PostCompactionContextContribution::default(),
                Err(err) => {
                    tracing::warn!(
                        thread_id = %runtime.thread_id(),
                        "failed to reconstruct active goal after compaction: {err}"
                    );
                    return PostCompactionContextContribution::default();
                }
            };
            PostCompactionContextContribution::with_lease(
                vec![continuation_steering_item(
                    &crate::tool::protocol_goal_from_state(goal),
                )],
                goal_state_permit,
            )
        })
    }
}

impl<C> TurnLifecycleContributor for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };
            if !runtime.is_enabled() {
                return;
            }
            let _goal_state_permit = match runtime.goal_state_permit().await {
                Ok(permit) => permit,
                Err(err) => {
                    tracing::warn!(
                        "failed to lock goal state at turn start for {}: {err}",
                        runtime.thread_id()
                    );
                    return;
                }
            };

            let accounting = runtime.accounting_state();
            accounting.start_turn(
                input.turn_id,
                input.collaboration_mode.mode,
                input.token_usage_at_turn_start,
            );
            if matches!(
                input.collaboration_mode.mode,
                codex_protocol::config_types::ModeKind::Plan
            ) {
                accounting.clear_current_turn_goal();
                return;
            }
            let revision = runtime.goal_revision();
            let goal = match self
                .state_dbs
                .thread_goals()
                .get_thread_goal_with_skill_selections(runtime.thread_id())
                .await
            {
                Ok(goal) => goal,
                Err(err) => {
                    tracing::warn!(
                        "failed to restore goal skill selections for {}: {err}",
                        runtime.thread_id()
                    );
                    accounting.clear_current_turn_goal();
                    runtime.clear_goal_skill_activations_at_revision(
                        revision,
                        InactiveGoalHistory::Invalidate,
                    );
                    return;
                }
            };
            if let Some((goal, skill_selections)) = goal
                && goal.status == codex_state::ThreadGoalStatus::Active
            {
                if runtime.activate_goal_skill_selections_at_revision(
                    revision,
                    goal.goal_id.clone(),
                    skill_selections,
                ) {
                    accounting.mark_turn_goal_active(input.turn_id, goal.goal_id);
                    if !runtime.goal_revision_is(revision) || !runtime.is_enabled() {
                        accounting.clear_current_turn_goal();
                    }
                }
            } else {
                runtime.clear_goal_skill_activations_at_revision(
                    revision,
                    InactiveGoalHistory::Preserve,
                );
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };
            if !runtime.is_enabled() {
                return;
            }

            let turn_id = input.turn_store.level_id();
            if let Err(err) = runtime
                .account_active_goal_progress(
                    turn_id,
                    &format!("{turn_id}:turn-stop"),
                    codex_state::GoalAccountingMode::ActiveOnly,
                    BudgetLimitedGoalDisposition::ClearActive,
                )
                .await
            {
                tracing::warn!(
                    "failed to account active goal progress at turn stop for {turn_id}: {err}"
                );
                return;
            }
            runtime.accounting_state().finish_turn(turn_id);
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };
            if !runtime.is_enabled() {
                return;
            }

            let turn_id = input.turn_store.level_id();
            if let Err(err) = runtime
                .account_active_goal_progress(
                    turn_id,
                    &format!("{turn_id}:turn-abort"),
                    codex_state::GoalAccountingMode::ActiveOnly,
                    BudgetLimitedGoalDisposition::ClearActive,
                )
                .await
            {
                tracing::warn!(
                    "failed to account active goal progress after turn abort for {turn_id}: {err}"
                );
                return;
            }
            runtime.accounting_state().finish_turn(turn_id);
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };

            let reason = match input.error {
                CodexErrorInfo::UsageLimitExceeded => ActiveGoalStopReason::UsageLimit,
                // The turn has ended because the error was non-retryable or its
                // retries were exhausted. Block the goal to prevent automatic
                // continuation from looping and consuming tokens, as can happen
                // with compaction errors.
                _ => ActiveGoalStopReason::TurnError,
            };
            if let Err(err) = runtime
                .stop_active_goal_for_turn(input.turn_id, reason)
                .await
            {
                tracing::warn!(
                    error = ?input.error,
                    "failed to stop active goal after turn error: {err}"
                );
            }
        })
    }
}

impl<C> TokenUsageContributor for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_token_usage<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        token_usage: &'a TokenUsageInfo,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(thread_store) else {
                return;
            };
            if !runtime.is_enabled() {
                return;
            }

            let Some(_recorded) = runtime
                .accounting_state()
                .record_token_usage(turn_store.level_id(), &token_usage.total_token_usage)
            else {
                return;
            };
        })
    }
}

impl<C> ToolLifecycleContributor for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let Some(runtime) = goal_runtime_handle(input.thread_store) else {
                return;
            };
            let should_count_for_goal_progress = runtime.is_enabled()
                && tool_attempt_counts_for_goal_progress(input.outcome)
                && !(input.tool_name.namespace.is_none()
                    && input.tool_name.name == UPDATE_GOAL_TOOL_NAME);
            if !should_count_for_goal_progress {
                return;
            }
            let turn_id = input.turn_id;
            let progress = match runtime
                .account_active_goal_progress(
                    turn_id,
                    input.call_id,
                    codex_state::GoalAccountingMode::ActiveOnly,
                    BudgetLimitedGoalDisposition::KeepActive,
                )
                .await
            {
                Ok(Some(progress)) => progress,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(
                        "failed to account active goal progress after tool finish for {turn_id}: {err}"
                    );
                    return;
                }
            };
            let goal = progress.goal;
            if goal.status != ThreadGoalStatus::BudgetLimited {
                return;
            }
            if !runtime
                .accounting_state()
                .mark_budget_limit_reported_if_new(progress.goal_id.as_str())
            {
                return;
            }
            let item = budget_limit_steering_item(&goal);
            runtime.inject_active_turn_steering(item).await;
        })
    }
}

impl<C> ToolContributor for GoalExtension<C>
where
    C: Send + Sync + 'static,
{
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>>> {
        let Some(runtime) = goal_runtime_handle(thread_store) else {
            return Vec::new();
        };
        if !runtime.tools_visible() {
            return Vec::new();
        }

        vec![
            Arc::new(GoalToolExecutor::get(
                runtime.as_ref().clone(),
                Arc::clone(&self.state_dbs),
                self.analytics.clone(),
                self.event_emitter.clone(),
                self.metrics.clone(),
            )),
            Arc::new(GoalToolExecutor::create(
                runtime.as_ref().clone(),
                Arc::clone(&self.state_dbs),
                self.analytics.clone(),
                self.event_emitter.clone(),
                self.metrics.clone(),
            )),
            Arc::new(GoalToolExecutor::update(
                runtime.as_ref().clone(),
                Arc::clone(&self.state_dbs),
                self.analytics.clone(),
                self.event_emitter.clone(),
                self.metrics.clone(),
            )),
        ]
    }
}

pub fn install_with_backend<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    state_dbs: Arc<codex_state::StateRuntime>,
    analytics_events_client: AnalyticsEventsClient,
    metrics_client: Option<MetricsClient>,
    thread_manager: Weak<ThreadManager>,
    goal_service: Arc<GoalService>,
    goals_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(GoalExtension::new_with_host_capabilities(
        state_dbs,
        analytics_events_client,
        registry.event_sink(),
        metrics_client,
        thread_manager,
        Arc::clone(&goal_service),
        goals_enabled,
    ));
    registry.thread_lifecycle_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.prompt_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.tool_contributor(extension);
}

fn goal_runtime_handle(thread_store: &ExtensionData) -> Option<Arc<GoalRuntimeHandle>> {
    thread_store.get::<GoalRuntimeHandle>()
}

fn tool_attempt_counts_for_goal_progress(outcome: ToolCallOutcome) -> bool {
    match outcome {
        ToolCallOutcome::Completed { .. } => true,
        ToolCallOutcome::Failed {
            handler_executed: true,
        } => true,
        ToolCallOutcome::Blocked
        | ToolCallOutcome::Failed {
            handler_executed: false,
        }
        | ToolCallOutcome::Aborted => false,
    }
}
