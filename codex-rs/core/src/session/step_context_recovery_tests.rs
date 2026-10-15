use super::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::update_selected_settings_for_test;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ReasoningSummary;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn recovery_snapshot_captures_effective_step_tier_and_summary() {
    let (_session, context) = make_session_and_context().await;
    let context = Arc::new(context);
    let mut expected = context
        .to_turn_context_item()
        .recovery_start_options
        .expect("new contexts record recovery options");
    expected.service_tier = Some("flex".to_string());
    expected.configured_reasoning_summary = Some(ReasoningSummary::Detailed);
    expected.reasoning_summary = Some(ReasoningSummary::Detailed);
    let mut step = StepContext::for_test(context);
    let step = Arc::get_mut(&mut step).expect("unique test step");
    let settings = Arc::make_mut(&mut step.settings);
    settings.service_tier = Some("flex".to_string());
    settings.reasoning_summary = ReasoningSummary::Detailed;
    update_selected_settings_for_test(settings, |selected| {
        selected.reasoning_summary = Some(ReasoningSummary::Detailed);
    });
    let item = step.to_turn_context_item();
    assert_eq!(
        (item.recovery_start_options, item.summary),
        (Some(expected), Some(ReasoningSummary::None))
    );
}

#[tokio::test]
async fn unrepresented_live_mode_change_disables_automatic_recovery() {
    let (_session, context) = make_session_and_context().await;
    let mut step = StepContext::for_test(Arc::new(context));
    let step = Arc::get_mut(&mut step).expect("unique test step");
    update_selected_settings_for_test(Arc::make_mut(&mut step.settings), |selected| {
        selected.collaboration_mode.mode = if selected.collaboration_mode.mode == ModeKind::Plan {
            ModeKind::Default
        } else {
            ModeKind::Plan
        };
    });
    assert_eq!(step.to_turn_context_item().recovery_start_options, None);
}

#[tokio::test]
async fn unrepresented_environment_constraint_change_disables_automatic_recovery() {
    let (_session, context) = make_session_and_context().await;
    let mut step = StepContext::for_test(Arc::new(context));
    let step = Arc::get_mut(&mut step).expect("unique test step");
    let environment = step
        .environments
        .environments
        .iter_mut()
        .find_map(|state| match state {
            crate::environment_selection::TurnEnvironmentState::Ready(environment) => {
                Some(environment)
            }
            _ => None,
        })
        .expect("fixture has a ready environment");
    let config = environment.config_mut();
    config.allow_login_shell = !config.allow_login_shell;
    assert_eq!(step.to_turn_context_item().recovery_start_options, None);
}
