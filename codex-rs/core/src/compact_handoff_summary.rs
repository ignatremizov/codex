//! Decodes a remote checkpoint for presentation without changing canonical model history.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::codex_delegate::compaction::DecoderRequest;
use crate::config::Config;
use crate::config::Constrained;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use tokio_util::sync::CancellationToken;

const HANDOFF_PROMPT: &str =
    "Repeat the compacted handoff content verbatim. Do not summarize, explain, or add any text.";
const DEFAULT_HANDOFF_MODEL: &str = "gpt-5.3-codex-spark";
const DEFAULT_HANDOFF_FALLBACK_MODEL: &str = "gpt-5.6-luna";
const HANDOFF_HELPER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, PartialEq, Eq)]
struct HandoffModelSelection {
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemoteCompactionHandoff {
    Skipped,
    Decoded(String),
    Failed(String),
}

pub(crate) fn should_decode_remote_compaction_handoff(config: &Config) -> bool {
    config.remote_compaction_handoff_enabled && config.features.enabled(Feature::RemoteCompaction)
}

pub(crate) async fn summarize_remote_compaction_handoff(
    sess: &Arc<Session>,
    turn: &Arc<TurnContext>,
    installed_history: &[ResponseItemEnvelope],
    cancellation: &CancellationToken,
) -> RemoteCompactionHandoff {
    if cancellation.is_cancelled() || !should_decode_remote_compaction_handoff(&turn.config) {
        return RemoteCompactionHandoff::Skipped;
    }
    let catalog = sess
        .services
        .models_manager
        .list_models(RefreshStrategy::Offline, turn.config.http_client_factory())
        .await;
    let current = turn.model_info();
    let primary = select_handoff_model(
        turn.config.remote_compaction_handoff_model.as_deref(),
        &catalog,
        &current.slug,
        turn.reasoning_effort()
            .cloned()
            .or(current.default_reasoning_level.clone()),
        current
            .supported_reasoning_levels
            .iter()
            .any(|preset| preset.effort == ReasoningEffort::Low),
    );
    let fallback = select_handoff_fallback_model(
        turn.config
            .remote_compaction_handoff_fallback_model
            .as_deref(),
        &catalog,
        &primary,
    );
    let mut errors = Vec::new();
    for selection in std::iter::once(primary).chain(fallback) {
        if cancellation.is_cancelled() {
            return RemoteCompactionHandoff::Skipped;
        }
        let result = async {
            let config = build_remote_compaction_handoff_config(&turn.config, &selection)?;
            let message = crate::codex_delegate::compaction::run(DecoderRequest {
                config,
                parent: Arc::clone(sess),
                turn: Arc::clone(turn),
                history: build_handoff_initial_history(installed_history),
                cancellation: cancellation.clone(),
                deadline: tokio::time::Instant::now() + HANDOFF_HELPER_TIMEOUT,
            })
            .await?;
            usable_handoff_message(message)
                .ok_or_else(|| anyhow::anyhow!("decoder returned no usable text"))
        }
        .await;
        if cancellation.is_cancelled() {
            return RemoteCompactionHandoff::Skipped;
        }
        match result {
            Ok(message) => return RemoteCompactionHandoff::Decoded(message),
            Err(error) => {
                tracing::warn!(turn_id = %turn.sub_id, model = %selection.model, %error,
                    "remote compaction handoff decoder failed");
                errors.push(format!(
                    "decoder model `{}` failed: {error:#}",
                    selection.model
                ));
                if error.is::<crate::codex_delegate::compaction::DecoderWorkerLost>() {
                    // No actual termination evidence: never overlap another decoder actor.
                    break;
                }
            }
        }
    }
    RemoteCompactionHandoff::Failed(errors.join("\n"))
}

fn usable_handoff_message(message: String) -> Option<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() || trimmed == HANDOFF_PROMPT {
        None
    } else {
        Some(message)
    }
}

fn build_handoff_initial_history(history: &[ResponseItemEnvelope]) -> InitialHistory {
    let mut items = history
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    items.push(RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: HANDOFF_PROMPT.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    InitialHistory::Forked(items)
}

fn select_handoff_model(
    configured: Option<&str>,
    catalog: &[ModelPreset],
    current: &str,
    current_effort: Option<ReasoningEffort>,
    current_supports_low: bool,
) -> HandoffModelSelection {
    let model = configured
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .or_else(|| {
            catalog
                .iter()
                .any(|preset| preset.model == DEFAULT_HANDOFF_MODEL)
                .then_some(DEFAULT_HANDOFF_MODEL)
        })
        .unwrap_or(current);
    let reasoning_effort = match catalog.iter().find(|preset| preset.model == model) {
        Some(preset) => Some(reasoning_effort_for_preset(preset)),
        None if model == current => {
            if current_supports_low {
                Some(ReasoningEffort::Low)
            } else {
                current_effort
            }
        }
        None => None,
    };
    HandoffModelSelection {
        model: model.to_string(),
        reasoning_effort,
    }
}

fn select_handoff_fallback_model(
    configured: Option<&str>,
    catalog: &[ModelPreset],
    primary: &HandoffModelSelection,
) -> Option<HandoffModelSelection> {
    let model = configured
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .or_else(|| {
            catalog
                .iter()
                .any(|preset| preset.model == DEFAULT_HANDOFF_FALLBACK_MODEL)
                .then_some(DEFAULT_HANDOFF_FALLBACK_MODEL)
        })?;
    (model != primary.model).then(|| HandoffModelSelection {
        model: model.to_string(),
        reasoning_effort: catalog
            .iter()
            .find(|preset| preset.model == model)
            .map(reasoning_effort_for_preset),
    })
}

fn reasoning_effort_for_preset(preset: &ModelPreset) -> ReasoningEffort {
    if preset
        .supported_reasoning_efforts
        .iter()
        .any(|preset| preset.effort == ReasoningEffort::Low)
    {
        ReasoningEffort::Low
    } else {
        preset.default_reasoning_effort.clone()
    }
}

fn build_remote_compaction_handoff_config(
    parent: &Config,
    selection: &HandoffModelSelection,
) -> anyhow::Result<Config> {
    let mut config = parent.clone();
    config.ephemeral = true;
    config.remote_compaction_handoff_enabled = false;
    config.model = Some(selection.model.clone());
    config.model_reasoning_effort = selection.reasoning_effort.clone();
    config.model_reasoning_summary = Some(ReasoningSummary::None);
    config.base_instructions = Some(HANDOFF_PROMPT.to_string());
    config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
    config.developer_instructions = None;
    config.personality = None;
    config.compact_prompt = None;
    config.project_doc_max_bytes = 0;
    config.project_doc_fallback_filenames = Vec::new();
    config.notify = None;
    config.include_permissions_instructions = false;
    config.include_apps_instructions = false;
    config.include_collaboration_mode_instructions = false;
    config.include_skill_instructions = false;
    config.include_environment_context = false;
    config.experimental_request_user_input_enabled = false;
    config.model_post_turn_compact_threshold_percent = 0;
    config.web_search_mode.set(WebSearchMode::Disabled)?;
    config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())?;
    config.mcp_servers.set(HashMap::new())?;
    // ToolPolicy provides the absolute tool ceiling; these also suppress side effects and
    // instruction/discovery paths that run before tool selection.
    for feature in [
        Feature::CodexHooks,
        Feature::CodeModePrewarm,
        Feature::RemoteCompaction,
        Feature::ContextManagement,
        Feature::TokenBudget,
        Feature::Goals,
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::Apps,
        Feature::Plugins,
        Feature::RemotePlugin,
    ] {
        config.features.disable(feature)?;
    }
    Ok(config)
}

#[cfg(test)]
#[path = "compact_handoff_summary_tests.rs"]
mod tests;
