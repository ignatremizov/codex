use super::helpers::make_chatwidget_manual;
use crate::chatwidget::ThreadInputStateRestoreMode;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn unresolved_draft_keeps_resolved_model_and_explicit_mode_choices() {
    for model_override in [None, Some(""), Some(" \t ")] {
        let (mut chat, _events, _operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        let mut resolved = chat.current_collaboration_mode.clone();
        resolved.settings.reasoning_effort = Some(ReasoningEffort::High);
        chat.current_collaboration_mode.settings.model.clear();
        chat.active_collaboration_mask = model_override.map(|model| CollaborationModeMask {
            name: "Plan".to_string(),
            mode: Some(ModeKind::Plan),
            model: Some(model.to_string()),
            reasoning_effort: Some(Some(ReasoningEffort::Low)),
            developer_instructions: None,
        });
        chat.apply_external_edit("continue the saved work".to_string());
        let saved = chat.capture_thread_input_state();
        let expected = chat
            .active_collaboration_mask
            .clone()
            .map(|mut mask| {
                mask.model = None;
                resolved.apply_mask(&mask)
            })
            .unwrap_or_else(|| resolved.clone());

        chat.current_collaboration_mode = resolved;
        chat.active_collaboration_mask = None;
        chat.restore_thread_input_state(
            saved,
            ThreadInputStateRestoreMode {
                preserve_in_flight_turn: false,
            },
        );

        assert_eq!(
            (
                chat.effective_collaboration_mode(),
                chat.composer_text_with_pending(),
            ),
            (expected, "continue the saved work".to_string()),
            "saved model override: {model_override:?}",
        );
    }
}

#[tokio::test]
async fn resolved_draft_retains_its_selected_settings() {
    let (mut chat, _events, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.apply_external_edit("keep my selected model".to_string());
    let expected = chat.effective_collaboration_mode();
    let saved = chat.capture_thread_input_state();

    chat.current_collaboration_mode.settings.model = "different-session-model".to_string();
    chat.active_collaboration_mask = None;
    chat.restore_thread_input_state(
        saved,
        ThreadInputStateRestoreMode {
            preserve_in_flight_turn: false,
        },
    );

    assert_eq!(
        (
            chat.effective_collaboration_mode(),
            chat.composer_text_with_pending(),
        ),
        (expected, "keep my selected model".to_string()),
    );
}
