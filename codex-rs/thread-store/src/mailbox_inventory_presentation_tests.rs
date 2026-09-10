use super::super::BoundedInventoryContext;
use super::*;
use crate::MailboxSender;
use pretty_assertions::assert_eq;

fn notification(sender_count: usize) -> MailboxInventoryNotification {
    let mut pending_senders = (0..sender_count)
        .map(|_| MailboxSenderInventory {
            sender: MailboxSender::Agent(ThreadId::new()),
            count: 2,
            max_acceptance_sequence: 10,
        })
        .collect::<Vec<_>>();
    pending_senders.sort_by_key(|sender| sender.sender.key());
    MailboxInventoryNotification {
        id: ThreadId::new().to_string(),
        receiver_thread_id: ThreadId::new(),
        through_sequence: 10,
        pending_senders,
    }
}

#[test]
fn recognizes_both_durable_formats_without_inventing_omitted_senders() {
    for count in [3, 32] {
        let notification = notification(count);
        let canonical = notification.context().expect("bounded canonical inventory");
        let original = canonical.clone();
        let view = MailboxInventoryContextView::from_item(
            &canonical.item,
            notification.receiver_thread_id,
        )
        .expect("recognized inventory text");
        let prefix = count.min(MAX_INVENTORY_CONTEXT_SENDERS);
        assert_eq!(view.pending_senders, notification.pending_senders[..prefix]);
        assert_eq!(view.omitted, count > prefix);
        assert_eq!(canonical, original);

        let legacy = notification
            .legacy_context()
            .expect("retained full inventory");
        let view =
            MailboxInventoryContextView::from_item(&legacy.item, notification.receiver_thread_id)
                .expect("legacy representation remains recognized");
        assert_eq!(view.pending_senders, notification.pending_senders);
        assert!(!view.omitted);
        assert!(MailboxInventoryContextView::from_item(&legacy.item, ThreadId::new()).is_none());
    }
}

#[test]
fn inconsistent_bounded_formats_are_not_projected() {
    let notification = notification(32);
    let original = notification.context().expect("bounded canonical inventory");
    for mutation in 0..7 {
        let mut item = original.item.clone();
        let ResponseItem::Message { content, .. } = &mut item else {
            panic!("inventory message");
        };
        let [ContentItem::InputText { text }] = content.as_mut_slice() else {
            panic!("inventory text");
        };
        let body = text.lines().nth(1).expect("snapshot JSON");
        let mut snapshot: BoundedInventoryContext =
            serde_json::from_str(body).expect("bounded snapshot");
        match mutation {
            0 => snapshot.format_version = 3,
            1 => snapshot.omitted_sender_count = 0,
            2 => snapshot.snapshot_sha256 = "not-a-sha256".to_string(),
            3 => snapshot.through_sequence = 0,
            4 => snapshot.pending_senders[0].count = 0,
            5 => snapshot.pending_senders.swap(0, 1),
            6 => {
                snapshot.pending_senders.pop();
            }
            _ => unreachable!(),
        }
        *text =
            inventory_context_text(&serde_json::to_string(&snapshot).expect("snapshot encoding"));
        assert!(
            MailboxInventoryContextView::from_item(&item, notification.receiver_thread_id)
                .is_none()
        );
    }
    assert_eq!(
        notification.context().expect("unchanged canonical format"),
        original
    );
}
