//! Keep saved chat choices within the latest successfully fetched provider catalog.
//! Running steps retain their captured settings; changes apply to the next turn.

use super::session::Session;
use super::session::SessionSettingsUpdate;
use super::step_settings::StepSettings;
use super::thread_settings;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use std::sync::Arc;

pub(super) async fn apply(session: &Session) {
    let settings_guard = thread_settings::acquire_persistence_lock(session).await;
    if session.check_history_publication().is_err() {
        return;
    }
    let Ok(Some(commit)) = session
        .update_settings_if_with_permit(
            SessionSettingsUpdate::default(),
            |current, updated| current.step_settings != updated.step_settings,
            &settings_guard,
        )
        .await
    else {
        return;
    };
    if let Err(error) = thread_settings::emit_applied(
        session,
        settings_guard,
        super::new_submission_id(),
        commit.snapshot,
        /*reply*/ None,
    )
    .await
    {
        tracing::warn!("failed to publish refreshed model settings: {error}");
    }
}

pub(super) fn normalize(settings: &mut Arc<StepSettings>, catalog: &[ModelInfo]) {
    let Some(model) = catalog
        .iter()
        .find(|model| model.slug == settings.collaboration_mode.model())
    else {
        return;
    };
    let effort = settings.collaboration_mode.reasoning_effort();
    let allowed = &model.supported_reasoning_levels;
    if let Some(effort) = effort
        && !allowed.is_empty()
        && !allowed.iter().any(|preset| preset.effort == effort)
    {
        let replacement = effort_rank(&effort)
            .and_then(|previous| {
                allowed
                    .iter()
                    .filter_map(|preset| {
                        effort_rank(&preset.effort)
                            .filter(|rank| *rank <= previous)
                            .map(|rank| (rank, &preset.effort))
                    })
                    .max_by_key(|(rank, _)| *rank)
                    .map(|(_, effort)| effort)
            })
            .or_else(|| {
                matches!(effort, ReasoningEffort::Ultra | ReasoningEffort::Persistent)
                    .then(|| model.default_reasoning_level.as_ref())
                    .flatten()
                    .filter(|default| allowed.iter().any(|preset| &preset.effort == *default))
            });
        if let Some(replacement) = replacement {
            Arc::make_mut(settings)
                .collaboration_mode
                .settings
                .reasoning_effort = Some(replacement.clone());
        }
    }

    if let Some(tier) = settings.service_tier.as_deref()
        && matches!(tier, "priority" | "ultrafast")
        && !model.supports_service_tier(tier)
    {
        Arc::make_mut(settings).service_tier = Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.into());
    }
}

fn effort_rank(effort: &ReasoningEffort) -> Option<usize> {
    match effort {
        ReasoningEffort::None => Some(0),
        ReasoningEffort::Minimal => Some(1),
        ReasoningEffort::Low => Some(2),
        ReasoningEffort::Medium => Some(3),
        ReasoningEffort::High => Some(4),
        ReasoningEffort::XHigh => Some(5),
        ReasoningEffort::Max => Some(6),
        ReasoningEffort::Ultra | ReasoningEffort::Persistent | ReasoningEffort::Custom(_) => None,
    }
}

#[cfg(test)]
#[path = "model_catalog_settings_tests.rs"]
mod tests;
