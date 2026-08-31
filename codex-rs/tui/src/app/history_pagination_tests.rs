use super::reconcile_older_turns;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

fn item(id: &str, text: &str) -> ThreadItem {
    ThreadItem::AgentMessage {
        id: id.to_string(),
        text: text.to_string(),
        phase: None,
        memory_citation: None,
        delivery: None,
        questions: None,
    }
}

fn turn(id: &str, items: Vec<ThreadItem>) -> Turn {
    Turn {
        id: id.to_string(),
        items,
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: Some(1),
        completed_at: Some(2),
        duration_ms: Some(1000),
    }
}

#[test]
fn page_completion_preserves_live_payloads_state_and_new_turns() {
    let live = item("live", "completed while page was loading");
    let newest = turn("new-turn", vec![item("new", "live new turn")]);
    let mut current = vec![turn("existing", vec![live.clone()]), newest.clone()];
    let old = item("old", "older page");
    let incoming = vec![
        turn("old-turn", vec![item("oldest", "oldest")]),
        Turn {
            status: TurnStatus::InProgress,
            completed_at: None,
            ..turn(
                "existing",
                vec![old.clone(), item("live", "stale"), old.clone()],
            )
        },
    ];
    assert_eq!(
        reconcile_older_turns(
            &mut current,
            incoming.clone(),
            &HashSet::from(["existing".to_string(), "new-turn".to_string()]),
        ),
        HashSet::from(["old".to_string(), "oldest".to_string()])
    );
    let expected = vec![
        turn("old-turn", vec![item("oldest", "oldest")]),
        turn("existing", vec![old, live]),
        newest,
    ];
    assert_eq!(current, expected);
    assert_eq!(
        reconcile_older_turns(
            &mut current,
            incoming,
            &HashSet::from(["existing".to_string(), "new-turn".to_string()]),
        ),
        HashSet::new()
    );
    assert_eq!(current, expected);
}

#[test]
fn large_retained_turn_keeps_payload_allocations_when_prepending_a_page() {
    let retained = (0..10_000)
        .map(|index| item(&format!("retained-{index}"), "retained payload"))
        .collect::<Vec<_>>();
    let mut current = vec![turn("turn", retained)];
    let allocations = current[0]
        .items
        .iter()
        .map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => text.as_ptr(),
            _ => unreachable!("fixture contains only agent messages"),
        })
        .collect::<Vec<_>>();
    let older = (0..100)
        .map(|index| item(&format!("older-{index}"), "page payload"))
        .collect::<Vec<_>>();
    let expected_ids = older
        .iter()
        .map(|item| item.id().to_string())
        .collect::<HashSet<_>>();
    assert_eq!(
        reconcile_older_turns(
            &mut current,
            vec![turn("turn", older.clone())],
            &HashSet::from(["turn".to_string()]),
        ),
        expected_ids
    );
    assert_eq!(&current[0].items[..older.len()], older.as_slice());
    let retained_allocations = current[0].items[older.len()..]
        .iter()
        .map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => text.as_ptr(),
            _ => unreachable!("fixture contains only agent messages"),
        })
        .collect::<Vec<_>>();
    assert_eq!(retained_allocations, allocations);
}

#[test]
fn page_completion_does_not_restore_a_snapshotted_turn_removed_while_loading() {
    let removed = turn("removed", vec![item("stale", "stale page payload")]);
    let older = turn("older", vec![item("older-item", "older page payload")]);
    let mut current = vec![turn("current", vec![item("current-item", "live")])];

    assert_eq!(
        reconcile_older_turns(
            &mut current,
            vec![older.clone(), removed],
            &HashSet::from(["removed".to_string(), "current".to_string()]),
        ),
        HashSet::from(["older-item".to_string()]),
    );
    assert_eq!(
        current,
        vec![older, turn("current", vec![item("current-item", "live")])]
    );
}
