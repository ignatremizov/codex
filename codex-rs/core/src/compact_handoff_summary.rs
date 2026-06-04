use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::codex_delegate::run_codex_thread_one_shot_with_environment_selections;
use crate::config::Config;
use crate::config::Constrained;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_features::Feature;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SubAgentSource;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const HANDOFF_PROMPT: &str =
    "Repeat the compacted handoff content verbatim. Do not summarize, explain, or add any text.";
const DEFAULT_HANDOFF_MODEL: &str = "gpt-5.3-codex-spark";
const HANDOFF_HELPER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffModelSelection {
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

pub(crate) async fn summarize_remote_compaction_handoff(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    new_history: &[ResponseItem],
    cancellation_token: &CancellationToken,
) -> Option<String> {
    if !should_decode_remote_compaction_handoff(turn_context.config.as_ref()) {
        return None;
    }

    match summarize_remote_compaction_handoff_inner(
        sess,
        turn_context,
        new_history,
        cancellation_token,
    )
    .await
    {
        Ok(message) => message,
        Err(err) => {
            warn!(turn_id = %turn_context.sub_id, error = %err, "failed to decode remote compaction handoff");
            None
        }
    }
}

pub(crate) fn should_decode_remote_compaction_handoff(config: &Config) -> bool {
    config.remote_compaction_handoff_enabled && config.features.enabled(Feature::RemoteCompaction)
}

async fn summarize_remote_compaction_handoff_inner(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    new_history: &[ResponseItem],
    cancellation_token: &CancellationToken,
) -> CodexResult<Option<String>> {
    let available_models = sess
        .services
        .models_manager
        .list_models(
            RefreshStrategy::Offline,
            turn_context.config.http_client_factory(),
        )
        .await;
    let selection = select_handoff_model(
        turn_context
            .config
            .remote_compaction_handoff_model
            .as_deref(),
        &available_models,
        turn_context.model_info.slug.as_str(),
        turn_context.reasoning_effort.clone(),
        turn_context.model_info.default_reasoning_level.clone(),
        turn_context
            .model_info
            .supported_reasoning_levels
            .iter()
            .any(|preset| preset.effort == ReasoningEffort::Low),
    );
    let helper_config =
        build_remote_compaction_handoff_config(turn_context.config.as_ref(), &selection).map_err(
            |err| CodexErr::Fatal(format!("remote compaction handoff config error: {err}")),
        )?;
    let initial_history = InitialHistory::Forked(build_handoff_initial_history(new_history));
    let deadline = tokio::time::Instant::now() + HANDOFF_HELPER_TIMEOUT;
    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout);
    let helper_cancel = cancellation_token.child_token();
    let spawn_helper = run_codex_thread_one_shot_with_environment_selections(
        helper_config,
        Arc::clone(&sess.services.auth_manager),
        sess.services.models_manager.clone(),
        Vec::new(),
        Arc::clone(sess),
        Arc::clone(turn_context),
        helper_cancel.clone(),
        SubAgentSource::Compact,
        /*final_output_json_schema*/ None,
        Some(initial_history),
        TurnEnvironmentSnapshot::default(),
    );
    let helper = tokio::select! {
        _ = &mut timeout => {
            helper_cancel.cancel();
            return Ok(None);
        }
        _ = cancellation_token.cancelled() => {
            helper_cancel.cancel();
            return Ok(None);
        }
        helper = spawn_helper => helper?,
    };

    loop {
        tokio::select! {
            _ = &mut timeout => {
                helper_cancel.cancel();
                return Ok(None);
            }
            _ = cancellation_token.cancelled() => {
                helper_cancel.cancel();
                return Ok(None);
            }
            event = helper.next_event() => {
                match event {
                    Ok(event) => match event.msg {
                        EventMsg::TurnComplete(turn_complete) => {
                            return Ok(turn_complete.last_agent_message.and_then(usable_handoff_message));
                        }
                        EventMsg::TurnAborted(_) => return Ok(None),
                        _ => {}
                    },
                    Err(_) => {
                        helper_cancel.cancel();
                        return Ok(None);
                    }
                }
            }
        }
    }
}

fn usable_handoff_message(message: String) -> Option<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() || trimmed == HANDOFF_PROMPT {
        None
    } else {
        Some(message)
    }
}

fn build_handoff_initial_history(new_history: &[ResponseItem]) -> Vec<RolloutItem> {
    let mut items = new_history
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    items.push(RolloutItem::ResponseItem(handoff_instruction_item()));
    items
}

fn handoff_instruction_item() -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: HANDOFF_PROMPT.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn select_handoff_model(
    configured_model: Option<&str>,
    available_models: &[ModelPreset],
    current_model: &str,
    current_reasoning_effort: Option<ReasoningEffort>,
    current_default_reasoning_effort: Option<ReasoningEffort>,
    current_supports_low_reasoning: bool,
) -> HandoffModelSelection {
    let model = configured_model
        .filter(|model| !model.trim().is_empty())
        .map(str::trim)
        .or_else(|| {
            available_models
                .iter()
                .any(|preset| preset.model == DEFAULT_HANDOFF_MODEL)
                .then_some(DEFAULT_HANDOFF_MODEL)
        })
        .unwrap_or(current_model);
    let catalog_preset = available_models.iter().find(|preset| preset.model == model);
    let reasoning_effort = match catalog_preset {
        Some(preset) => reasoning_effort_for_preset(preset),
        None if model == current_model => {
            if current_supports_low_reasoning {
                Some(ReasoningEffort::Low)
            } else {
                current_reasoning_effort.or(current_default_reasoning_effort)
            }
        }
        None => None,
    };

    HandoffModelSelection {
        model: model.to_string(),
        reasoning_effort,
    }
}

fn reasoning_effort_for_preset(preset: &ModelPreset) -> Option<ReasoningEffort> {
    if preset
        .supported_reasoning_efforts
        .iter()
        .any(|preset| preset.effort == ReasoningEffort::Low)
    {
        Some(ReasoningEffort::Low)
    } else {
        Some(preset.default_reasoning_effort.clone())
    }
}

pub(crate) fn build_remote_compaction_handoff_config(
    parent_config: &Config,
    selection: &HandoffModelSelection,
) -> anyhow::Result<Config> {
    let mut helper_config = parent_config.clone();
    helper_config.ephemeral = true;
    helper_config.remote_compaction_handoff_enabled = false;
    helper_config.model = Some(selection.model.clone());
    helper_config.model_reasoning_effort = selection.reasoning_effort.clone();
    helper_config.base_instructions = Some(HANDOFF_PROMPT.to_string());
    helper_config.developer_instructions = None;
    helper_config.personality = None;
    helper_config.compact_prompt = None;
    helper_config.project_doc_max_bytes = 0;
    helper_config.project_doc_fallback_filenames = Vec::new();
    helper_config.notify = None;
    helper_config.include_permissions_instructions = false;
    helper_config.include_apps_instructions = false;
    helper_config.include_collaboration_mode_instructions = false;
    helper_config.include_skill_instructions = false;
    helper_config.include_environment_context = false;
    helper_config.experimental_request_user_input_enabled = false;
    helper_config.web_search_mode.set(WebSearchMode::Disabled)?;
    helper_config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    helper_config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())?;
    helper_config.mcp_servers.set(HashMap::new())?;
    for feature in [
        Feature::ShellTool,
        Feature::UnifiedExec,
        Feature::ShellZshFork,
        Feature::UnifiedExecZshFork,
        Feature::CodeMode,
        Feature::CodeModeOnly,
        Feature::Personality,
        Feature::CodexHooks,
        Feature::RequestPermissionsTool,
        Feature::RemoteCompaction,
        Feature::RemoteCompactionV2,
        Feature::WebSearchRequest,
        Feature::WebSearchCached,
        Feature::StandaloneWebSearch,
        Feature::Goals,
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::SpawnCsv,
        Feature::Apps,
        Feature::EnableMcpApps,
        Feature::Plugins,
        Feature::RemotePlugin,
        Feature::ToolSuggest,
        Feature::ImageGeneration,
        Feature::BrowserUse,
        Feature::BrowserUseExternal,
        Feature::ComputerUse,
    ] {
        helper_config.features.disable(feature).map_err(|err| {
            anyhow::anyhow!(
                "remote compaction handoff helper could not disable `features.{}`: {err}",
                feature.key()
            )
        })?;
    }
    Ok(helper_config)
}

#[cfg(test)]
#[path = "compact_handoff_summary_tests.rs"]
mod tests;
