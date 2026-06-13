use super::super::tests::new_test_composer;
use crate::render::renderable::Renderable;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[test]
fn meter_labels_preserve_mouse_selection_before_and_after_placeholder() {
    use crate::bottom_pane::textarea::TextAreaState;
    use crossterm::event::KeyModifiers;
    use crossterm::event::MouseButton;
    use crossterm::event::MouseEvent;
    use crossterm::event::MouseEventKind;
    use ratatui::widgets::StatefulWidgetRef;

    for selected in ["before ", "after"] {
        let (mut composer, _events) = new_test_composer();
        composer.draft.textarea.insert_str("before ");
        let id = composer.begin_dictation();
        composer.draft.textarea.insert_str(" after");
        let start = composer
            .draft
            .textarea
            .text()
            .find(selected)
            .expect("selection");
        let end = start + selected.len();
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 120, /*height*/ 1,
        );
        let mut buffer = Buffer::empty(area);
        let mut state = TextAreaState::default();
        StatefulWidgetRef::render_ref(&&composer.draft.textarea, area, &mut buffer, &mut state);
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), start),
            (MouseEventKind::Drag(MouseButton::Left), end),
            (MouseEventKind::Up(MouseButton::Left), end),
        ] {
            assert!(composer.draft.textarea.handle_mouse(
                MouseEvent {
                    kind,
                    column: u16::try_from(column).expect("column"),
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                state
            ));
        }
        for label in [
            "[Dictation: recording |]",
            "[Dictation: recording |||||]",
            "[Dictation: recording |||||]",
            "[Dictation: transcribing]",
        ] {
            assert!(composer.update_dictation(id, "", label));
            let start = composer
                .draft
                .textarea
                .text()
                .find(selected)
                .expect("selected text");
            let range = start..start + selected.len();
            assert_eq!(
                composer.draft.textarea.mouse_selection_range(),
                Some(range.clone())
            );
            assert_eq!(&composer.draft.textarea.text()[range], selected);
        }
        composer.draft.textarea.insert_str("replacement");
        assert!(!composer.draft.textarea.text().contains(selected));
        assert!(composer.has_dictation_element(id));
    }
}

#[test]
fn pending_dictation_accepts_file_completion_without_submitting() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    for (code, mentions_v2) in [
        (KeyCode::Enter, false),
        (KeyCode::Tab, false),
        (KeyCode::Enter, true),
        (KeyCode::Tab, true),
    ] {
        let (mut composer, _events) = new_test_composer();
        composer.set_disable_paste_burst(/*disabled*/ true);
        composer.set_mentions_v2_enabled(mentions_v2);
        let id = composer.begin_dictation();
        composer.insert_str(" @dict");
        composer.on_file_search_result(
            "dict".into(),
            vec![codex_file_search::FileMatch {
                score: 1,
                path: std::path::PathBuf::from("dictated.rs"),
                match_type: codex_file_search::MatchType::File,
                root: crate::test_support::test_path_buf("/tmp"),
                indices: None,
            }],
        );
        let (result, _) = composer.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(matches!(result, super::super::super::InputResult::None));
        assert_eq!(
            composer.draft.textarea.text(),
            "[Dictation: preparing] dictated.rs "
        );
        assert!(composer.has_dictation_element(id));
        let (result, _) =
            composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(result, super::super::super::InputResult::None));
    }
}

#[test]
fn history_search_acceptance_does_not_resurrect_dictation_marker() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    for (key, expected) in [(KeyCode::Enter, "remembered"), (KeyCode::Esc, "original ")] {
        let (mut composer, _events) = new_test_composer();
        composer.history.record_local_submission(
            crate::bottom_pane::chat_composer_history::HistoryEntry::new("remembered".into()),
        );
        composer.insert_str("original ");
        let id = composer.begin_dictation();
        composer.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(composer.history_search.is_some());
        assert!(!composer.has_dictation_element(id));
        composer.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let (result, _) = composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(matches!(result, super::super::super::InputResult::None));
        assert!(composer.history_search.is_none());
        assert_eq!(composer.draft.textarea.text(), expected);
    }
}

#[test]
fn pending_slash_enter_and_immediate_tab_complete_without_dispatch_or_draft_loss() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    for (typed, completed, key) in [
        ("/sta", "/status", KeyCode::Enter),
        ("/ski", "/skills", KeyCode::Tab),
    ] {
        let (mut composer, _events) = new_test_composer();
        composer.set_disable_paste_burst(/*disabled*/ true);
        composer.insert_str(&format!("{typed} "));
        let id = composer.begin_dictation();
        composer.insert_str(" tail");
        composer.draft.textarea.set_cursor(typed.len());
        composer.sync_popups();
        assert!(matches!(
            composer.popups.active,
            super::super::ActivePopup::Command(_)
        ));
        let (result, _) = composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(matches!(result, super::super::super::InputResult::None));
        assert_eq!(
            composer.draft.textarea.text(),
            format!("{completed} [Dictation: preparing] tail")
        );
        assert!(composer.has_dictation_element(id));
    }
}

#[test]
fn pending_dictation_composer_snapshot() {
    let (mut composer, _events) = new_test_composer();
    composer.begin_dictation();
    let width = 60;
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        width,
        composer.desired_height(width),
    );
    let mut buffer = Buffer::empty(area);
    composer.render(area, &mut buffer);
    let rows = buffer
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rows.trim(), @"
    › [Dictation: preparing]

    100% context left
    ");
}

#[test]
fn submission_is_held_but_typed_text_and_newlines_remain_editable() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    let (mut composer, _events) = new_test_composer();
    let id = composer.begin_dictation();
    composer.draft.textarea.insert_str("typed\ntext");
    for code in [KeyCode::Enter, KeyCode::Tab] {
        let (result, _) = composer.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(matches!(result, super::super::super::InputResult::None));
    }
    assert!(composer.update_dictation(id, "spoken ", "[Dictation: transcribing]"));
    composer.finish_dictation(id);
    assert_eq!(composer.draft.textarea.text(), "spoken typed\ntext");
}

#[test]
fn pending_dictation_preserves_paste_burst_enter_and_tab() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    let (mut composer, _events) = new_test_composer();
    let id = composer.begin_dictation();
    let now = std::time::Instant::now();
    composer
        .draft
        .paste_burst
        .begin_with_retro_grabbed("a".into(), now);
    for code in [KeyCode::Enter, KeyCode::Tab] {
        let (result, _) = composer.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(matches!(result, super::super::super::InputResult::None));
    }
    composer.handle_paste_burst_flush(now + std::time::Duration::from_secs(/*secs*/ 1));
    composer.finish_dictation(id);
    assert_eq!(composer.draft.textarea.text(), "a\n\t");
}

#[test]
fn dictation_is_exact_id_owned_and_never_submits() {
    let (mut composer, _events) = new_test_composer();
    composer.draft.textarea.insert_str("typed ");
    let unrelated = composer
        .draft
        .textarea
        .insert_element("[Dictation: preparing]");
    let id = composer.begin_dictation();
    assert!(composer.update_dictation(id, "日本語 ", "[Dictation: transcribing]"));
    composer.finish_dictation(id);
    assert_eq!(
        composer.draft.textarea.text(),
        "typed [Dictation: preparing]日本語 "
    );
    assert_eq!(
        composer.draft.textarea.text_element_snapshots()[0].id,
        unrelated
    );
    insta::assert_snapshot!(composer.draft.textarea.text(), @"typed [Dictation: preparing]日本語 ");
}

#[test]
fn deleted_element_and_old_id_cannot_edit_a_new_draft() {
    let (mut composer, _events) = new_test_composer();
    let old = composer.begin_dictation();
    composer.finish_dictation(old);
    let new = composer.begin_dictation();
    assert!(!composer.update_dictation(old, "stale", "stale"));
    assert!(composer.has_dictation_element(new));
    composer.finish_dictation(new);
    assert_eq!(composer.draft.textarea.text(), "");
}
