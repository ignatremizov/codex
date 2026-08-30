use super::*;
use crate::app_event::AppEvent;
use crate::bottom_pane::AppEventSender;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

fn composer_with_targets(text: &str, cursor: usize) -> ChatComposer {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        /*has_input_focus*/ true,
        AppEventSender::new(tx),
        /*enhanced_keys_supported*/ false,
        "Ask Codex to do anything".to_string(),
        /*disable_paste_burst*/ false,
    );
    composer.set_agent_prompt_targets(vec![
        AgentPromptTarget {
            thread_id: Some(
                ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad")
                    .expect("valid thread id"),
            ),
            selector: "2".to_string(),
            label: "Robie [explorer]".to_string(),
        },
        AgentPromptTarget {
            thread_id: Some(
                ThreadId::from_string("019fbb08-bb4e-79e4-afdb-77de9737bebe")
                    .expect("valid thread id"),
            ),
            selector: "3".to_string(),
            label: "Herschel [worker]".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "close".to_string(),
            label: "Close an agent".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "observe".to_string(),
            label: "Change response observation".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "reviewer".to_string(),
            label: "New reviewer agent".to_string(),
        },
    ]);
    let primary_model = ModelPreset {
        id: "gpt-5.6-sol".to_string(),
        model: "gpt-5.6-sol".to_string(),
        display_name: "GPT-5.6 Sol".to_string(),
        description: "Frontier coding model".to_string(),
        model_specialty: None,
        default_reasoning_effort: ReasoningEffort::Medium,
        supported_reasoning_efforts: vec![
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffort::Medium,
                description: "Balanced reasoning".to_string(),
            },
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffort::High,
                description: "Deeper reasoning".to_string(),
            },
        ],
        supports_personality: false,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        is_default: true,
        upgrade: None,
        show_in_picker: true,
        multi_agent_version: None,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: codex_protocol::openai_models::default_input_modalities(),
    };
    let alternate_model = ModelPreset {
        id: "gpt-5.6-luna".to_string(),
        model: "gpt-5.6-luna".to_string(),
        display_name: "GPT-5.6 Luna".to_string(),
        description: "Fast coding model".to_string(),
        default_reasoning_effort: ReasoningEffort::Low,
        supported_reasoning_efforts: vec![
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffort::Low,
                description: "Fast reasoning".to_string(),
            },
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffort::Ultra,
                description: "Maximum reasoning".to_string(),
            },
        ],
        is_default: false,
        ..primary_model.clone()
    };
    composer.set_agent_spawn_models(vec![primary_model, alternate_model]);
    composer.draft.textarea.set_text_clearing_elements(text);
    composer.draft.textarea.set_cursor(cursor);
    composer.sync_popups();
    composer
}

#[test]
fn tab_completes_spawn_model_and_reasoning_options() {
    let mut composer =
        composer_with_targets("/agent new model:gpt-5", "/agent new model:gpt-5".len());

    let result = composer
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .0;

    assert_eq!(result, InputResult::None);
    assert_eq!(
        composer.draft.textarea.text(),
        "/agent new model:gpt-5.6-sol "
    );

    composer
        .draft
        .textarea
        .set_text_clearing_elements("/agent reviewer model:gpt-5.6-sol effort:h");
    composer
        .draft
        .textarea
        .set_cursor("/agent reviewer model:gpt-5.6-sol effort:h".len());
    composer.sync_popups();
    let result = composer
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .0;

    assert_eq!(result, InputResult::None);
    assert_eq!(
        composer.draft.textarea.text(),
        "/agent reviewer model:gpt-5.6-sol effort:high "
    );
}

#[test]
fn reasoning_completion_uses_selected_model_capabilities() {
    let composer =
        composer_with_targets("/agent reviewer effort:h", "/agent reviewer effort:h".len());
    assert!(matches!(composer.popups.active, ActivePopup::None));

    let mut composer = composer_with_targets(
        "/agent reviewer model:gpt-5.6-sol effort:u",
        "/agent reviewer model:gpt-5.6-sol effort:u".len(),
    );
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected reasoning-effort popup");
    };
    assert_eq!(popup.selected_target(), None);

    composer
        .draft
        .textarea
        .set_text_clearing_elements("/agent reviewer model:gpt-5.6-luna effort:u");
    composer
        .draft
        .textarea
        .set_cursor("/agent reviewer model:gpt-5.6-luna effort:u".len());
    composer.sync_popups();
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected reasoning-effort popup");
    };
    assert_eq!(
        popup.selected_target(),
        Some(AgentPromptTarget {
            thread_id: None,
            selector: "effort:ultra".to_string(),
            label: "Maximum reasoning".to_string(),
        })
    );

    composer
        .draft
        .textarea
        .set_text_clearing_elements("/agent reviewer effort:u model:gpt-5.6-sol");
    composer
        .draft
        .textarea
        .set_cursor("/agent reviewer effort:u".len());
    composer.sync_popups();
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected reasoning-effort popup");
    };
    assert_eq!(popup.selected_target(), None);
}

#[test]
fn completing_observe_target_advances_popup_to_observation_modes() {
    let mut composer =
        composer_with_targets("/agent observe 019faa", "/agent observe 019faa".len());

    let result = composer
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .0;

    assert_eq!(result, InputResult::None);
    assert_eq!(composer.draft.textarea.text(), "/agent observe 2 ");
    assert_eq!(composer.draft.textarea.cursor(), "/agent observe 2 ".len());
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected observation-mode popup");
    };
    assert_eq!(
        popup.selected_target(),
        Some(AgentPromptTarget {
            thread_id: None,
            selector: "passive".to_string(),
            label: "Deliver the final response without waking".to_string(),
        })
    );
}

#[test]
fn tab_completes_existing_target_after_agent_action() {
    let mut composer = composer_with_targets("/agent close 019faa", "/agent close 019faa".len());
    assert!(matches!(
        composer.popups.active,
        ActivePopup::AgentTarget(_)
    ));

    let result = composer
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .0;

    assert_eq!(result, InputResult::None);
    assert_eq!(composer.draft.textarea.text(), "/agent close 2 ");
    assert_eq!(composer.draft.textarea.cursor(), "/agent close 2 ".len());
    assert!(matches!(composer.popups.active, ActivePopup::None));
}

#[test]
fn completing_agent_action_advances_popup_to_existing_targets() {
    let mut composer = composer_with_targets("/agent clo", "/agent clo".len());

    let result = composer
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .0;

    assert_eq!(result, InputResult::None);
    assert_eq!(composer.draft.textarea.text(), "/agent close ");
    assert_eq!(composer.draft.textarea.cursor(), "/agent close ".len());
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected existing-target popup");
    };
    assert_eq!(
        popup.selected_target(),
        Some(AgentPromptTarget {
            thread_id: Some(
                ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad")
                    .expect("valid thread id"),
            ),
            selector: "2".to_string(),
            label: "Robie [explorer]".to_string(),
        })
    );
}

#[test]
fn tab_completes_agent_ref_and_preserves_prompt_tail() {
    let mut composer = composer_with_targets("/agent 019faa review this", "/agent 019faa".len());
    assert!(matches!(
        composer.popups.active,
        ActivePopup::AgentTarget(_)
    ));

    let result = composer
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .0;

    assert_eq!(result, InputResult::None);
    assert_eq!(composer.draft.textarea.text(), "/agent 2 review this");
    assert_eq!(composer.draft.textarea.cursor(), "/agent 2".len());
    assert!(matches!(composer.popups.active, ActivePopup::None));
}

#[test]
fn empty_agent_target_opens_active_target_popup() {
    let composer = composer_with_targets("/agent ", "/agent ".len());
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected agent target popup");
    };

    assert_eq!(
        popup.selected_target(),
        Some(AgentPromptTarget {
            thread_id: Some(
                ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad")
                    .expect("valid thread id"),
            ),
            selector: "2".to_string(),
            label: "Robie [explorer]".to_string(),
        })
    );
}

#[test]
fn empty_eligible_target_list_does_not_open_popup() {
    let mut composer = composer_with_targets("/agent ", "/agent ".len());
    composer.set_agent_prompt_targets(Vec::new());

    assert!(matches!(composer.popups.active, ActivePopup::None));
}

#[test]
fn nonempty_eligible_target_list_keeps_popup_open_when_query_has_no_matches() {
    let composer = composer_with_targets("/agent zzzz", "/agent zzzz".len());
    let ActivePopup::AgentTarget(popup) = &composer.popups.active else {
        panic!("expected agent target popup");
    };

    assert_eq!(popup.selected_target(), None);
}
