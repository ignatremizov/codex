use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn activity_between_subscription_and_read_is_retained_and_receiver_scoped()
-> anyhow::Result<()> {
    let (receiver, _) = make_session_and_context().await;
    let (other, _) = make_session_and_context().await;
    let mut subscribed = receiver.subscribe_mailbox_activity();
    let unrelated = other.subscribe_mailbox_activity();
    // An acceptance can commit here, before the waiting tool's first store read.
    receiver.notify_mailbox_activity();
    assert!(subscribed.has_changed()?);
    subscribed.changed().await?;
    assert!(!subscribed.has_changed()?);
    assert!(!unrelated.has_changed()?);
    // Hints coalesce but another same-valued notification still changes version.
    receiver.notify_mailbox_activity();
    receiver.notify_mailbox_activity();
    subscribed.changed().await?;
    assert!(!subscribed.has_changed()?);
    assert_eq!(
        (
            receiver.input_queue.has_pending_mailbox_items().await,
            receiver.input_queue.has_queued_turn_inputs().await,
            receiver.active_turn.lock().await.is_some(),
        ),
        (false, false, false),
    );
    Ok(())
}
