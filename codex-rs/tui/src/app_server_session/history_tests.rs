use super::advancing_cursor;
use super::prepend_item_page;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemEntry;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

#[test]
fn advancing_cursor_rejects_repeated_cursors() {
    let mut seen_cursors = HashSet::new();
    assert_eq!(
        advancing_cursor(
            /*current*/ None,
            Some("first".to_string()),
            &mut seen_cursors,
        ),
        Some("first".to_string())
    );
    assert_eq!(
        advancing_cursor(Some("first"), Some("second".to_string()), &mut seen_cursors,),
        Some("second".to_string())
    );
    assert_eq!(
        advancing_cursor(Some("second"), Some("first".to_string()), &mut seen_cursors,),
        None
    );
    assert_eq!(
        advancing_cursor(Some("second"), /*next*/ None, &mut seen_cursors),
        None
    );
}

fn item(id: &str) -> ThreadItem {
    ThreadItem::UserMessage {
        id: id.to_string(),
        client_id: None,
        content: Vec::new(),
    }
}

fn turn(id: &str, items: Vec<ThreadItem>) -> Turn {
    Turn {
        id: id.to_string(),
        items,
        items_view: TurnItemsView::Summary,
        status: TurnStatus::Completed,
        error: None,
        started_at: Some(1),
        completed_at: Some(2),
        duration_ms: Some(1000),
    }
}

#[test]
fn large_single_turn_page_preserves_order_and_existing_identity() {
    let older = (0..10_000)
        .map(|index| item(&format!("item-{index}")))
        .collect::<Vec<_>>();
    let retained = item("retained");
    let mut turns = vec![turn("turn", vec![retained.clone()])];
    let entries = older
        .iter()
        .rev()
        .cloned()
        .map(|item| ThreadItemEntry {
            turn_id: "turn".to_string(),
            item,
        })
        .collect();
    assert_eq!(prepend_item_page(&mut turns, entries), older);
    let mut expected = older;
    expected.push(retained);
    assert_eq!(turns, vec![turn("turn", expected)]);
}

#[test]
fn overlapping_pages_deduplicate_within_each_turn_and_page() {
    let mut turns = vec![
        turn("first", vec![item("shared")]),
        turn("second", vec![item("newest")]),
    ];
    let entries = [
        ("second", "shared"),
        ("second", "shared"),
        ("first", "shared"),
        ("first", "oldest"),
    ]
    .into_iter()
    .map(|(turn_id, id)| ThreadItemEntry {
        turn_id: turn_id.to_string(),
        item: item(id),
    })
    .collect::<Vec<_>>();
    assert_eq!(
        prepend_item_page(&mut turns, entries.clone()),
        vec![item("oldest"), item("shared")]
    );
    let expected = vec![
        turn("first", vec![item("oldest"), item("shared")]),
        turn("second", vec![item("shared"), item("newest")]),
    ];
    assert_eq!(turns, expected);
    assert_eq!(
        prepend_item_page(&mut turns, entries),
        Vec::<ThreadItem>::new()
    );
    assert_eq!(turns, expected);
}
