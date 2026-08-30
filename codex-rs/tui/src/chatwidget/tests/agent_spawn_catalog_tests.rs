//! Spawn completions follow accepted model-catalog updates, not stale account replies.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn accepted_model_catalog_refresh_updates_live_spawn_completions() {
    let (mut chat, _events, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut initial = get_available_model(&chat, "gpt-5.5");
    initial.id = "catalog-initial".to_string();
    initial.model = initial.id.clone();
    initial.show_in_picker = true;
    let initial_request = uuid::Uuid::new_v4();
    chat.model_popup_request_id = Some(initial_request);
    assert!(chat.on_models_loaded(initial_request, Ok(vec![initial.clone()])));

    chat.bottom_pane.set_composer_text(
        "/agent new model:catalog-".to_string(),
        Vec::new(),
        Vec::new(),
    );
    let mut latest = initial.clone();
    latest.id = "catalog-latest".to_string();
    latest.model = latest.id.clone();
    latest.default_reasoning_effort = ReasoningEffortConfig::Low;
    latest.supported_reasoning_efforts = vec![ReasoningEffortPreset {
        effort: ReasoningEffortConfig::Low,
        description: "Current catalog effort".to_string(),
    }];
    let current_request = uuid::Uuid::new_v4();
    chat.model_popup_request_id = Some(current_request);
    assert!(!chat.on_models_loaded(initial_request, Ok(vec![latest.clone()])));
    assert_eq!(
        chat.model_catalog.try_list_models().expect("catalog"),
        vec![initial]
    );
    assert!(chat.on_models_loaded(current_request, Ok(vec![latest.clone()])));
    assert_eq!(
        chat.model_catalog.try_list_models().expect("catalog"),
        vec![latest]
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        chat.bottom_pane.composer_text(),
        "/agent new model:catalog-latest "
    );
    chat.bottom_pane.set_composer_text(
        "/agent new model:catalog-latest effort:l".to_string(),
        Vec::new(),
        Vec::new(),
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    insta::assert_snapshot!(chat.bottom_pane.composer_text(), @"/agent new model:catalog-latest effort:low ");
}
