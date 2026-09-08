use super::*;
use crate::mailbox_inventory_artifacts::canonical_context;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

fn notification(sender_count: usize) -> MailboxInventoryNotification {
    let mut pending_senders = (0..sender_count)
        .map(|_| MailboxSenderInventory {
            sender: MailboxSender::Agent(ThreadId::new()),
            count: u64::MAX,
            max_acceptance_sequence: i64::MAX,
        })
        .collect::<Vec<_>>();
    pending_senders.sort_by_key(|sender| sender.sender.key());
    MailboxInventoryNotification {
        id: ThreadId::new().to_string(),
        receiver_thread_id: ThreadId::new(),
        through_sequence: i64::MAX,
        pending_senders,
    }
}

fn text(envelope: &ResponseItemEnvelope) -> &str {
    let ResponseItem::Message { content, .. } = &envelope.item else {
        panic!("inventory message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("single inventory fragment");
    };
    text
}

#[test]
fn inventory_projection_is_bounded_without_changing_snapshot_or_frontier() {
    for count in [1, 8, 9, 4_096] {
        let notification = notification(count);
        let before = notification.clone();
        let context = notification.context().expect("bounded inventory");
        assert!(text(&context).len() <= MAX_INVENTORY_CONTEXT_BYTES);
        assert_eq!(notification, before);
        let snapshot: serde_json::Value =
            serde_json::from_str(text(&context).lines().nth(1).expect("snapshot line"))
                .expect("snapshot JSON");
        assert_eq!(snapshot["through_sequence"], i64::MAX);
        assert_eq!(
            snapshot["pending_senders"]
                .as_array()
                .expect("sender rows")
                .len(),
            count.min(MAX_INVENTORY_CONTEXT_SENDERS)
        );
        if count <= MAX_INVENTORY_CONTEXT_SENDERS {
            assert_eq!(
                context,
                notification
                    .legacy_context()
                    .expect("unchanged small context")
            );
        } else {
            assert_eq!(snapshot["format_version"], 2);
            assert_eq!(
                snapshot["omitted_sender_count"],
                count - MAX_INVENTORY_CONTEXT_SENDERS
            );
            assert_eq!(
                snapshot["snapshot_sha256"]
                    .as_str()
                    .expect("snapshot commitment")
                    .len(),
                64
            );
            assert!(text(&context).contains("no filter to consume all pending mail"));
        }
    }
}

#[test]
fn omitted_sender_changes_cannot_match_an_existing_inventory_proof() {
    let original = notification(10);
    let context = original.context().expect("bounded inventory");
    let history = [RolloutItem::ResponseItem(context.clone())];
    assert_eq!(
        canonical_context(&original, &history).expect("original proof"),
        Some(context)
    );
    let mut altered = original;
    altered.pending_senders[9].count -= 1;
    assert!(canonical_context(&altered, &history).is_err());
}

#[test]
fn historical_full_inventory_is_recognized_without_rewriting_or_weakening_conflicts() {
    let notification = notification(32);
    let mut legacy = notification
        .legacy_context()
        .expect("original full inventory");
    legacy
        .item
        .set_create_time_if_missing(serde_json::Number::from(42));
    let history = [RolloutItem::ResponseItem(legacy.clone())];
    assert_eq!(
        canonical_context(&notification, &history).expect("legacy proof"),
        Some(legacy.clone())
    );
    let mut altered = notification.clone();
    altered.pending_senders[31].count -= 1;
    assert!(canonical_context(&altered, &history).is_err());
    let bounded = notification.context().expect("new bounded inventory");
    assert!(
        canonical_context(
            &notification,
            &[
                RolloutItem::ResponseItem(legacy),
                RolloutItem::ResponseItem(bounded),
            ]
        )
        .is_err(),
        "two different canonical originals with one identity remain a conflict"
    );
}
