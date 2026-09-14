use super::*;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;

#[test]
fn authority_recheck_distinguishes_mismatch_from_read_failure() {
    let subscription = MailboxFinalSubscription {
        message_id: "mailbox-message".to_string(),
        receiver_thread_id: ThreadId::new(),
        sender_thread_id: ThreadId::new(),
        acceptance_sequence: 1,
        state: MailboxFinalSubscriptionState::Pending,
        bound_turn_id: None,
        receiver_lifecycle_epoch: 3,
        sender_lifecycle_epoch: 5,
    };
    assert!(matches!(
        mailbox_final_subscription_authority_matches(
            &subscription,
            Ok(MailboxFinalSubscriptionAuthority {
                receiver_lifecycle_epoch: 3,
                sender_lifecycle_epoch: 5,
            }),
        ),
        Ok(true)
    ));
    assert!(matches!(
        mailbox_final_subscription_authority_matches(
            &subscription,
            Ok(MailboxFinalSubscriptionAuthority {
                receiver_lifecycle_epoch: 4,
                sender_lifecycle_epoch: 5,
            }),
        ),
        Ok(false)
    ));

    let result = mailbox_final_subscription_authority_matches(
        &subscription,
        Err(CodexErr::Fatal("authority unavailable".to_string())),
    );
    assert!(matches!(
        result,
        Err(error)
            if matches!(
                error.details(),
                CodexErrorDetails::Fatal(message) if message == "authority unavailable"
            )
    ));
}
