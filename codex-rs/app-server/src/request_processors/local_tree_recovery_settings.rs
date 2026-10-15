//! Automatic recovery never rewrites newer persistent preferences to reproduce an old turn.

use codex_core::ThreadConfigSnapshot;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_path_uri::PathUri;
use std::io;

pub(super) fn validate_recovery_settings(
    context: &TurnContextItem,
    config: &ThreadConfigSnapshot,
) -> io::Result<()> {
    let saved = context.recovery_start_options.as_ref().ok_or_else(|| {
        io::Error::other(
            "original turn start options were not recorded; explicit retry is required",
        )
    })?;
    let mismatches = [
        ("model", context.model != config.model),
        (
            "collaboration mode",
            context.collaboration_mode.as_ref() != Some(&config.collaboration_mode),
        ),
        (
            "reasoning effort",
            context.effort != config.reasoning_effort,
        ),
        ("personality", context.personality != config.personality),
        (
            "disabled plugins",
            context.disabled_plugin_ids.as_ref() != Some(&config.disabled_plugin_ids),
        ),
        (
            "reasoning summary",
            // An unset selection is resolved and compared against the original effective
            // summary by Core before admission. Do not reject an equivalent model default.
            config.reasoning_summary.is_some()
                && config.reasoning_summary != saved.configured_reasoning_summary
                && config.reasoning_summary != saved.reasoning_summary.or(context.summary),
        ),
        (
            "service tier",
            saved.service_tier.is_none() && config.service_tier.is_some(),
        ),
    ]
    .into_iter()
    .filter_map(|(name, changed)| changed.then_some(name))
    .collect::<Vec<_>>();
    if !mismatches.is_empty() {
        return Err(io::Error::other(format!(
            "original turn settings differ from restored thread settings ({}); explicit retry is required",
            mismatches.join(", ")
        )));
    }
    let [environment] = config.environment_selections() else {
        return Err(io::Error::other(
            "recovery requires one persisted local environment",
        ));
    };
    if environment.environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID
        || environment.config != EnvironmentConfigState::FromThread
        || environment.cwd != PathUri::from_abs_path(&context.cwd)
        || context.cwd != *config.cwd()
        || context.permission_profile() != config.permission_profile
        || context.approval_policy != config.approval_policy
        || context.approvals_reviewer != Some(config.approvals_reviewer)
        || context.workspace_roots.as_ref() != Some(&config.workspace_roots)
    {
        return Err(io::Error::other(
            "saved permissions or environment cannot be restored exactly",
        ));
    }
    Ok(())
}
