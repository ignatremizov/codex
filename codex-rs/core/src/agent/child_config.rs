//! Prepares child configuration from captured step settings and requested overrides.
//!
//! Spawn and reload share live runtime policy. Model-authored history inheritance is
//! authorized only after the selected role has been applied.

use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::apply_role_to_config;
use crate::agent::types::SpawnAgentForkMode;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::BaseInstructions;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::MultiAgentVersion;

/// Selects the existing spawn tool's developer-instruction inheritance rules.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnConfigVersion {
    V1,
    V2,
}

/// Distinguishes user-authorized history inheritance from model-authored requests.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnConfigOrigin {
    Model,
    User,
}

pub(crate) struct SpawnConfigOptions<'a> {
    pub(crate) origin: SpawnConfigOrigin,
    pub(crate) version: SpawnConfigVersion,
    pub(crate) fork_mode: Option<&'a SpawnAgentForkMode>,
    pub(crate) role_name: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) service_tier: Option<&'a str>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

pub(crate) struct PreparedSpawnConfig {
    pub(crate) config: Config,
    pub(crate) role_name: Option<String>,
}

/// Resolves child settings before starting the thread, retaining the invoking tool's precedence.
pub(crate) async fn prepare_agent_spawn_config(
    session: &Session,
    step_context: &StepContext,
    options: SpawnConfigOptions<'_>,
) -> Result<PreparedSpawnConfig, String> {
    let turn = step_context.turn.as_ref();
    let mut config =
        build_agent_spawn_config(&session.get_base_instructions().await, step_context)?;
    apply_spawn_agent_role_and_model_overrides(
        session,
        turn,
        &mut config,
        options.role_name,
        options.model,
        options.reasoning_effort,
    )
    .await?;
    if options.origin == SpawnConfigOrigin::Model
        && options.fork_mode.is_some()
        && !config.agent_allow_history_forks
    {
        return Err(
            "Parent-history forks are disabled by user configuration. Spawn without inherited \
             history, or ask the user to set `agents.allow_history_forks = true` globally or in \
             the selected agent role config."
                .to_string(),
        );
    }
    if options.version == SpawnConfigVersion::V2
        && matches!(options.fork_mode, Some(SpawnAgentForkMode::FullHistory))
        && config.developer_instructions.is_none()
    {
        config
            .developer_instructions
            .clone_from(&turn.developer_instructions);
    }
    apply_spawn_agent_service_tier(
        session,
        &mut config,
        step_context.settings.requested_service_tier(),
        options.service_tier,
    )
    .await?;
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    // Remember an applied configured default so cold reload reapplies its restrictions.
    let role_name = options
        .role_name
        .or_else(|| {
            config
                .agent_roles
                .get(DEFAULT_ROLE_NAME)
                .is_some_and(|role| role.config_file.is_some())
                .then_some(DEFAULT_ROLE_NAME)
        })
        .map(str::to_owned);
    Ok(PreparedSpawnConfig { config, role_name })
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// model selection and reasoning settings captured for the invoking step, plus the turn's
/// runtime approval policy, sandbox, and cwd. Role-specific overrides are layered
/// after this step; skipping this helper and cloning stale config state directly can send the child
/// agent out with the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    step_context: &StepContext,
) -> Result<Config, String> {
    let mut config = build_agent_shared_config(step_context.turn.as_ref())?;
    let settings = &step_context.settings;
    config.model = Some(settings.model_info.slug.clone());
    config.service_tier = settings.requested_service_tier().map(str::to_owned);
    config.model_reasoning_effort = settings.effective_reasoning_effort();
    config.model_reasoning_summary = Some(settings.reasoning_summary);
    config.base_instructions = Some(base_instructions.text.clone());
    config.base_instructions_provenance = base_instructions.provenance.clone();
    Ok(config)
}

pub(crate) fn build_agent_resume_config(turn: &TurnContext) -> Result<Config, String> {
    let mut config = build_agent_shared_config(turn)?;
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, String> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    // Preserve activation for history forks without freezing the parent's model-owned prompts.
    // Fresh child startup restores configured preferences from the retained snapshot.
    config.token_budget = turn.configured_token_budget.clone();
    config.model = Some(turn.model_info().slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort()
        .or(turn.model_info().default_reasoning_level.as_ref())
        .cloned();
    config.model_reasoning_summary = Some(turn.reasoning_summary());
    config.developer_instructions = turn.developer_instructions.clone();
    if turn.multi_agent_version == MultiAgentVersion::V2
        && let Some(developer_instructions) = turn
            .config
            .multi_agent_v2
            .subagent_developer_instructions
            .clone()
    {
        config.developer_instructions = Some(developer_instructions);
    }
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

/// Copies runtime-only turn state onto a child config before it is handed to `LocalAgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale can
/// make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), String> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy())
        .map_err(|err| format!("approval_policy is invalid: {err}"))?;
    config.approvals_reviewer = turn.config.approvals_reviewer;
    #[allow(deprecated)]
    let turn_cwd = turn.cwd.clone();
    config.cwd = turn_cwd;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(
            turn.config
                .permissions
                .permission_profile_state()
                .snapshot(),
        )
        .map_err(|err| format!("permission_profile is invalid: {err}"))?;
    Ok(())
}

/// Applies configured defaults, the selected role, and explicit model settings in precedence
/// order, then validates the resolved child model and reasoning effort.
async fn apply_spawn_agent_role_and_model_overrides(
    session: &Session,
    turn: &TurnContext,
    config: &mut Config,
    role_name: Option<&str>,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), String> {
    apply_default_spawn_agent_model_overrides(turn, config);
    let role_application = apply_role_to_config(config, role_name).await?;
    if role_application.overrides_model && !role_application.overrides_reasoning_effort {
        config.model_reasoning_effort = None;
    }
    // A role's model is a default, but its custom base instructions define the selected role.
    // An explicit model override changes the former without silently discarding the latter.
    apply_requested_spawn_agent_model_overrides(
        session,
        config,
        requested_model,
        requested_reasoning_effort,
    )
    .await?;
    validate_spawn_agent_model_selection(session, config).await
}

fn apply_default_spawn_agent_model_overrides(turn: &TurnContext, config: &mut Config) {
    if let Some(model) = turn.config.agent_default_subagent_model.as_ref() {
        config.model = Some(model.clone());
        if turn
            .config
            .agent_default_subagent_reasoning_effort
            .is_none()
        {
            config.model_reasoning_effort = None;
        }
    }
    if let Some(reasoning_effort) = turn.config.agent_default_subagent_reasoning_effort.as_ref() {
        config.model_reasoning_effort = Some(reasoning_effort.clone());
    }
}

async fn apply_requested_spawn_agent_model_overrides(
    session: &Session,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), String> {
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = session
            .services
            .models_manager
            .list_models(RefreshStrategy::Offline, config.http_client_factory())
            .await;
        let selected_model_name = find_spawn_agent_model_name(&available_models, requested_model)?;
        config.model = Some(selected_model_name);
        config.model_reasoning_effort = requested_reasoning_effort;

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        config.model_reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

/// Resolves a missing effort from the final model and validates the final pair once all layers
/// have been applied.
async fn validate_spawn_agent_model_selection(
    session: &Session,
    config: &mut Config,
) -> Result<(), String> {
    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for reasoning effort validation".to_string()
    })?;
    let model_info = session
        .services
        .models_manager
        .get_model_info(&model, &config.to_models_manager_config())
        .await;
    let Some(reasoning_effort) = config.model_reasoning_effort.as_ref() else {
        config.model_reasoning_effort = model_info.default_reasoning_level;
        return Ok(());
    };
    if model_info.used_fallback_model_metadata {
        return Ok(());
    }
    validate_spawn_agent_reasoning_effort(
        &model,
        &model_info.supported_reasoning_levels,
        reasoning_effort,
    )
}

pub(crate) async fn apply_spawn_agent_service_tier(
    session: &Session,
    config: &mut Config,
    parent_service_tier: Option<&str>,
    requested_service_tier: Option<&str>,
) -> Result<(), String> {
    let candidate_service_tiers = [
        requested_service_tier.map(str::to_string),
        config.service_tier.clone(),
        parent_service_tier.map(str::to_string),
    ];
    if candidate_service_tiers.iter().all(Option::is_none) {
        config.service_tier = None;
        return Ok(());
    }

    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for service tier validation".to_string()
    })?;
    let model_info = session
        .services
        .models_manager
        .get_model_info(model.as_str(), &config.to_models_manager_config())
        .await;

    if let Some(requested_service_tier) = requested_service_tier
        && requested_service_tier != SERVICE_TIER_DEFAULT_REQUEST_VALUE
        && !model_info.supports_service_tier(requested_service_tier)
    {
        let supported_service_tiers = if model_info.service_tiers.is_empty() {
            "none".to_string()
        } else {
            model_info
                .service_tiers
                .iter()
                .map(|tier| tier.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(format!(
            "Service tier `{requested_service_tier}` is not supported for model `{model}`. Supported service tiers: {supported_service_tiers}"
        ));
    }

    config.service_tier =
        candidate_service_tiers
            .into_iter()
            .flatten()
            .find(|candidate_service_tier| {
                candidate_service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE
                    || model_info.supports_service_tier(candidate_service_tier)
            });
    Ok(())
}

fn find_spawn_agent_model_name(
    available_models: &[ModelPreset],
    requested_model: &str,
) -> Result<String, String> {
    available_models
        .iter()
        .find(|model| model.model == requested_model)
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            )
        })
}

fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: &ReasoningEffort,
) -> Result<(), String> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| &preset.effort == requested_reasoning_effort)
    {
        return Ok(());
    }

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    ))
}
