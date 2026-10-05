use super::*;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use pretty_assertions::assert_eq;

#[test]
fn receipt_identity_keeps_selected_arc_and_uses_client_id_when_media_was_redacted() {
    let mut prompt = history_cell::new_user_prompt(
        "inspect".into(),
        Vec::new(),
        vec!["image.png".into()],
        Vec::new(),
    );
    prompt.client_id = Some("submission-2".into());
    let selected: Arc<dyn HistoryCell> = Arc::new(prompt);
    let cells = vec![Arc::clone(&selected)];
    let identity = UserMessageIdentity {
        turn_id: "turn-2".into(),
        item_id: "user-2".into(),
    };
    attach_identity(&cells, identity.clone(), Some("submission-2"), &[]);
    attach_identity(
        &cells,
        UserMessageIdentity {
            turn_id: "other-turn".into(),
            item_id: "other-user".into(),
        },
        Some("submission-2"),
        &[],
    );
    assert!(Arc::ptr_eq(&selected, &cells[0]));
    let cell = selected
        .as_any()
        .downcast_ref::<UserHistoryCell>()
        .expect("user cell");
    assert_eq!(cell.identity.get(), Some(&identity));
    assert_eq!(
        cell.local_image_paths,
        vec![std::path::PathBuf::from("image.png")]
    );
}

#[test]
fn receipt_without_submission_id_cannot_claim_an_ambiguous_duplicate() {
    let cells: Vec<Arc<dyn HistoryCell>> = (0..2)
        .map(|_| {
            Arc::new(history_cell::new_user_prompt(
                "same".into(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )) as Arc<dyn HistoryCell>
        })
        .collect();
    attach_identity(
        &cells,
        UserMessageIdentity {
            turn_id: "turn".into(),
            item_id: "user".into(),
        },
        /*client_id*/ None,
        &[UserInput::Text {
            text: "same".into(),
            text_elements: Vec::new(),
        }],
    );
    assert_eq!(
        cells
            .iter()
            .map(|cell| {
                cell.as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .expect("user")
                    .identity
                    .get()
            })
            .collect::<Vec<_>>(),
        vec![None, None]
    );
}
