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
    composer.draft.textarea.set_text_clearing_elements(text);
    composer.draft.textarea.set_cursor(cursor);
    composer.sync_popups();
    composer
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
