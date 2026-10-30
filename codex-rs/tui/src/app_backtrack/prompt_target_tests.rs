use super::*;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;

fn turn(turn_id: &str, item_id: &str) -> Turn {
    Turn {
        root_turn_id: None,
        id: turn_id.into(),
        items: vec![ThreadItem::UserMessage {
            id: item_id.into(),
            client_id: None,
            content: vec![UserInput::Text {
                text: "same".into(),
                text_elements: Vec::new(),
            }],
        }],
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

#[test]
fn canonical_item_disambiguates_repeated_turn_ids_and_ignores_stale_display_text() {
    let turns = vec![turn("duplicate", "first"), turn("duplicate", "second")];
    let identity = UserMessageIdentity {
        turn_id: "duplicate".into(),
        item_id: "second".into(),
    };
    let selected = selected_prompt_target(
        &turns,
        Some(&identity),
        /*start_item*/ None,
        /*nth_user_message*/ 0,
        &mut UserMessage::from("stale projection"),
    )
    .expect("canonical item");
    assert_eq!(
        LegacyRollbackTarget::new(&turns, &selected).expect("target"),
        LegacyRollbackTarget {
            num_turns: 1,
            expected_start_turn_id: "duplicate".into(),
            expected_start_item_id: None,
            expected_turn_count: 2
        }
    );
}

#[test]
fn missing_canonical_item_never_falls_back_to_equal_text() {
    let identity = UserMessageIdentity {
        turn_id: "turn".into(),
        item_id: "missing".into(),
    };
    assert!(
        selected_prompt_target(
            &[turn("turn", "actual")],
            Some(&identity),
            /*start_item*/ None,
            /*nth_user_message*/ 0,
            &mut UserMessage::from("same"),
        )
        .is_err()
    );
}

#[test]
fn source_less_duplicate_cannot_claim_a_persisted_occurrence_by_ordinal() {
    assert!(
        selected_prompt_target(
            &[turn("first", "first-item"), turn("second", "second-item")],
            /*identity*/ None,
            /*start_item*/ None,
            /*nth_user_message*/ 0,
            &mut UserMessage::from("same"),
        )
        .is_err()
    );
}

#[test]
fn canonical_steer_identity_selects_the_exact_item_boundary() {
    let mut first = turn("turn", "first");
    first.items.extend(turn("turn", "steer").items);
    let identity = UserMessageIdentity {
        turn_id: "turn".into(),
        item_id: "steer".into(),
    };
    let turns = vec![first, turn("later", "later-item")];
    let selected = selected_prompt_target(
        &turns,
        Some(&identity),
        /*start_item*/ None,
        /*nth_user_message*/ 0,
        &mut UserMessage::from("same"),
    )
    .expect("select steer");
    assert_eq!(
        selected,
        PromptEditTarget {
            turn_index: 0,
            before_item_id: Some("steer".into()),
        }
    );
    assert_eq!(
        LegacyRollbackTarget::new(&turns, &selected).expect("guarded steer target"),
        LegacyRollbackTarget {
            num_turns: 2,
            expected_start_turn_id: "turn".into(),
            expected_start_item_id: Some("steer".into()),
            expected_turn_count: 2,
        }
    );
}

#[test]
fn canonical_identity_cannot_edit_an_active_turn() {
    let mut active = turn("turn", "first");
    active.status = TurnStatus::InProgress;
    let identity = UserMessageIdentity {
        turn_id: "turn".into(),
        item_id: "first".into(),
    };
    assert!(
        selected_prompt_target(
            &[active],
            Some(&identity),
            /*start_item*/ None,
            /*nth_user_message*/ 0,
            &mut UserMessage::from("same"),
        )
        .is_err()
    );
}

#[test]
fn clipped_transcript_resolves_the_steer_against_the_complete_turn() {
    let mut original = turn("turn", "initial");
    let mut steer = turn("turn", "steer");
    let ThreadItem::UserMessage { content, .. } = &mut steer.items[0] else {
        unreachable!()
    };
    *content = vec![UserInput::Text {
        text: "correction".into(),
        text_elements: Vec::new(),
    }];
    original.items.extend(steer.items);
    let target = selected_prompt_target(
        &[original],
        /*identity*/ None,
        Some(&("turn".into(), "steer".into())),
        /*nth_user_message*/ 0,
        &mut UserMessage::from("correction"),
    )
    .expect("steer at the loaded boundary");
    assert_eq!(
        target,
        PromptEditTarget {
            turn_index: 0,
            before_item_id: Some("steer".into()),
        },
    );
}
