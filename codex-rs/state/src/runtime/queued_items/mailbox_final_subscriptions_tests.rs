use super::super::tests::assert_invalid_input;
use super::super::tests::invocation;
use super::super::tests::runtime;
use super::super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn replacing_final_intent_preserves_claim_identity_and_captured_authority() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    let sender_key = format!("agent:{sender}");
    let authority = MailboxFinalSubscriptionAuthority {
        receiver_lifecycle_epoch: 3,
        sender_lifecycle_epoch: 5,
    };
    let first = queue
        .accept_mail_with_final_subscription_and_authority(
            receiver,
            "first",
            &sender_key,
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
            Some(authority),
        )
        .await
        .unwrap();
    let call = invocation(receiver, "fixed");
    let mut original_claim = queue
        .claim_mail(&call, &MailboxSelection::All)
        .await
        .unwrap();
    let second = queue
        .accept_mail_with_final_subscription_and_authority(
            receiver,
            "second",
            &sender_key,
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
            Some(MailboxFinalSubscriptionAuthority {
                receiver_lifecycle_epoch: 4,
                sender_lifecycle_epoch: 6,
            }),
        )
        .await
        .unwrap();
    let mut expected = first;
    expected.state = MailboxMessageState::Claimed;
    expected.final_subscription.as_mut().unwrap().state = MailboxFinalSubscriptionState::Superseded;
    original_claim.messages[0].message = expected.clone();
    assert_eq!(
        queue
            .claim_mail(&call, &MailboxSelection::All)
            .await
            .unwrap(),
        original_claim,
    );
    // An idempotent retry cannot refresh captured authority or replace newer intent.
    assert_eq!(
        queue
            .accept_mail_with_final_subscription_and_authority(
                receiver,
                "first",
                &sender_key,
                "{}",
                MailboxFinalSubscriptionRequest::Wake,
                Some(MailboxFinalSubscriptionAuthority {
                    receiver_lifecycle_epoch: 9,
                    sender_lifecycle_epoch: 9,
                }),
            )
            .await
            .unwrap(),
        expected,
    );
    assert_invalid_input(
        queue
            .acknowledge_mailbox_final_subscription_delivery(receiver, &expected.id, &call.turn_id)
            .await
            .unwrap_err(),
    );
    queue
        .supersede_mailbox_final_subscription_message(receiver, &expected.id)
        .await
        .unwrap();
    assert_eq!(
        queue
            .read_active_mailbox_final_subscription(receiver, sender)
            .await
            .unwrap(),
        second.final_subscription,
    );
    let config = runtime.sqlite().clone();
    runtime.close().await;
    let reopened = crate::StateRuntime::init(config, "test-provider".to_string())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .thread_queue()
            .lookup_mail_claim(&call)
            .await
            .unwrap(),
        Some(original_claim),
    );
}

#[tokio::test]
async fn final_subscription_acceptance_is_idempotent_and_latest_fresh_acceptance_wins() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    let sender_key = format!("agent:{sender}");
    let first = queue
        .accept_mail_with_final_subscription(
            receiver,
            "first",
            &sender_key,
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    let first_subscription = first.final_subscription.clone().unwrap();
    assert_eq!(
        first_subscription.state,
        MailboxFinalSubscriptionState::Pending
    );

    assert_eq!(
        first,
        queue
            .accept_mail_with_final_subscription(
                receiver,
                "first",
                &sender_key,
                "{}",
                MailboxFinalSubscriptionRequest::Wake,
            )
            .await
            .unwrap()
    );
    queue
        .accept_mail(receiver, "legacy-z", &sender_key, "{}")
        .await
        .unwrap();
    assert_eq!(
        Some(first_subscription.clone()),
        queue
            .read_active_mailbox_final_subscription(receiver, sender)
            .await
            .unwrap()
    );

    let second = queue
        .accept_mail_with_final_subscription(
            receiver,
            "second",
            &sender_key,
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    let mut superseded = first_subscription;
    superseded.state = MailboxFinalSubscriptionState::Superseded;
    assert_eq!(
        Some(superseded),
        queue
            .read_mail(receiver, &first.id)
            .await
            .unwrap()
            .unwrap()
            .final_subscription
    );
    assert_eq!(
        second.final_subscription,
        queue
            .read_active_mailbox_final_subscription(receiver, sender)
            .await
            .unwrap()
    );

    let retried_old = queue
        .accept_mail_with_final_subscription(
            receiver,
            "first",
            &sender_key,
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    assert_eq!(retried_old.id, first.id);
    assert_eq!(
        second.final_subscription,
        queue
            .read_active_mailbox_final_subscription(receiver, sender)
            .await
            .unwrap()
    );
    assert_invalid_input(
        queue
            .accept_mail_with_final_subscription(
                receiver,
                "first",
                &sender_key,
                "{}",
                MailboxFinalSubscriptionRequest::None,
            )
            .await
            .unwrap_err(),
    );
}

#[tokio::test]
async fn consumption_binds_final_subscription_to_its_exact_turn_and_keeps_terminal_evidence() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    let accepted = queue
        .accept_mail_with_final_subscription(
            receiver,
            "subscription",
            &format!("agent:{sender}"),
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    let claim = queue
        .claim_mail(&invocation(receiver, "check-mail"), &MailboxSelection::All)
        .await
        .unwrap();
    let consumed = queue
        .acknowledge_mail(
            &claim.invocation,
            &accepted.id,
            &claim.messages[0].delivery_id,
        )
        .await
        .unwrap();
    let subscription = consumed.final_subscription.unwrap();
    assert_eq!(subscription.state, MailboxFinalSubscriptionState::Bound);
    assert_eq!(
        subscription.bound_turn_id.as_deref(),
        Some(claim.invocation.turn_id.as_str())
    );

    queue
        .acknowledge_mailbox_final_subscription_delivery(
            receiver,
            &accepted.id,
            &claim.invocation.turn_id,
        )
        .await
        .unwrap();
    let delivered = queue
        .read_mail(receiver, &accepted.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        delivered.final_subscription,
        Some(MailboxFinalSubscription {
            message_id: accepted.id,
            receiver_thread_id: receiver,
            sender_thread_id: sender,
            acceptance_sequence: 1,
            state: MailboxFinalSubscriptionState::Delivered,
            bound_turn_id: Some(claim.invocation.turn_id),
            receiver_lifecycle_epoch: 0,
            sender_lifecycle_epoch: 0,
        })
    );
}

#[tokio::test]
async fn rejecting_unannounced_mail_rejects_only_its_pending_subscription() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    let accepted = queue
        .accept_mail_with_final_subscription(
            receiver,
            "subscription",
            &format!("agent:{sender}"),
            "{}",
            MailboxFinalSubscriptionRequest::Wake,
        )
        .await
        .unwrap();
    let rejected = queue
        .reject_mail(receiver, &accepted.id, "permission revoked")
        .await
        .unwrap();
    assert_eq!(
        rejected.final_subscription,
        Some(MailboxFinalSubscription {
            message_id: accepted.id,
            receiver_thread_id: receiver,
            sender_thread_id: sender,
            acceptance_sequence: 1,
            state: MailboxFinalSubscriptionState::Rejected,
            bound_turn_id: None,
            receiver_lifecycle_epoch: 0,
            sender_lifecycle_epoch: 0,
        })
    );
}
