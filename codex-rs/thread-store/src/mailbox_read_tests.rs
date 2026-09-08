use super::claim_params;
use super::stores;
use super::user_submission;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::MailboxSelection;
use crate::MailboxSender;
use crate::RejectMailboxInputParams;
use crate::ThreadStore;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn claim_lookup_is_read_only_and_preserves_empty_and_terminal_batches() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let mut params = claim_params(receiver, "original");
        assert_eq!(
            store
                .lookup_mailbox_claim(params.invocation.clone())
                .await
                .unwrap(),
            None
        );
        let first = store
            .accept_mailbox_input(user_submission(receiver, "first"))
            .await
            .unwrap();
        // A missing lookup must not have established an empty claim.
        params.selection =
            MailboxSelection::Senders(vec![MailboxSender::User, MailboxSender::User]);
        let mut original = store.claim_mailbox_input(params.clone()).await.unwrap();
        assert_eq!(original.messages.len(), 1);
        assert_eq!(
            original.selection,
            MailboxSelection::Senders(vec![MailboxSender::User])
        );
        let empty_params = claim_params(receiver, "empty");
        let empty = store
            .claim_mailbox_input(empty_params.clone())
            .await
            .unwrap();
        assert!(empty.messages.is_empty());
        store
            .accept_mailbox_input(user_submission(receiver, "later"))
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_mailbox_claim(empty_params.invocation)
                .await
                .unwrap(),
            Some(empty)
        );
        assert_eq!(
            store
                .lookup_mailbox_claim(params.invocation.clone())
                .await
                .unwrap(),
            Some(original.clone())
        );
        for invocation in [
            crate::MailboxInvocation {
                receiver_thread_id: ThreadId::new(),
                ..params.invocation.clone()
            },
            crate::MailboxInvocation {
                turn_id: "other-turn".to_string(),
                ..params.invocation.clone()
            },
            crate::MailboxInvocation {
                tool_call_id: "other-call".to_string(),
                ..params.invocation.clone()
            },
        ] {
            assert_eq!(store.lookup_mailbox_claim(invocation).await.unwrap(), None);
        }
        for invocation in [
            crate::MailboxInvocation {
                turn_id: String::new(),
                ..params.invocation.clone()
            },
            crate::MailboxInvocation {
                tool_call_id: String::new(),
                ..params.invocation.clone()
            },
        ] {
            assert!(store.lookup_mailbox_claim(invocation).await.is_err());
        }
        original.messages[0].message = store
            .reject_mailbox_input(RejectMailboxInputParams {
                receiver_thread_id: receiver,
                message_id: first.id,
                reason: "not admitted".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_mailbox_claim(params.invocation.clone())
                .await
                .unwrap(),
            Some(original)
        );
        params.selection = MailboxSelection::All;
        assert!(store.claim_mailbox_input(params).await.is_err());
        let inventory = store.read_mailbox_inventory(receiver).await.unwrap();
        assert_eq!(inventory.pending_senders[0].count, 1);
        assert!(inventory.claimed_senders.is_empty());
    }
}

#[tokio::test]
async fn local_claim_lookup_recovers_fixed_batches_after_reopen() {
    let home = tempfile::tempdir().unwrap();
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "test-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let receiver = ThreadId::new();
    let empty = store
        .claim_mailbox_input(claim_params(receiver, "empty"))
        .await
        .unwrap();
    store
        .accept_mailbox_input(user_submission(receiver, "original"))
        .await
        .unwrap();
    let original = store
        .claim_mailbox_input(claim_params(receiver, "original"))
        .await
        .unwrap();
    store
        .accept_mailbox_input(user_submission(receiver, "later"))
        .await
        .unwrap();
    drop(store);
    drop(runtime);
    let reopened_runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let reopened = LocalThreadStore::new(config, Some(reopened_runtime));
    for claim in [empty, original] {
        assert_eq!(
            reopened
                .lookup_mailbox_claim(claim.invocation.clone())
                .await
                .unwrap(),
            Some(claim)
        );
    }
    assert_eq!(
        reopened
            .lookup_mailbox_claim(claim_params(receiver, "absent").invocation)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn pending_frontier_ignores_newer_same_sender_and_other_receivers() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        store
            .accept_mailbox_input(user_submission(ThreadId::new(), "foreign"))
            .await
            .unwrap();
        store
            .accept_mailbox_input(user_submission(receiver, "original"))
            .await
            .unwrap();
        let notification = store
            .prepare_mailbox_inventory(receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .has_pending_mailbox_inventory(notification.clone())
                .await
                .unwrap()
        );
        let claim = store
            .claim_mailbox_input(claim_params(receiver, "claim"))
            .await
            .unwrap();
        store
            .accept_mailbox_input(user_submission(receiver, "later"))
            .await
            .unwrap();
        let before = store.read_mailbox_inventory(receiver).await.unwrap();
        assert_eq!(
            before.pending_senders[0].count,
            notification.pending_senders[0].count
        );
        assert!(before.pending_senders[0].max_acceptance_sequence > notification.through_sequence);
        assert!(
            !store
                .has_pending_mailbox_inventory(notification.clone())
                .await
                .unwrap()
        );
        assert_eq!(
            store.read_mailbox_inventory(receiver).await.unwrap(),
            before
        );
        store
            .reject_mailbox_input(RejectMailboxInputParams {
                receiver_thread_id: receiver,
                message_id: claim.messages[0].message.id.clone(),
                reason: "not admitted".to_string(),
            })
            .await
            .unwrap();
        assert!(
            !store
                .has_pending_mailbox_inventory(notification.clone())
                .await
                .unwrap()
        );
        let after = store.read_mailbox_inventory(receiver).await.unwrap();
        assert_eq!(after.active_notification, Some(notification.clone()));
        assert_eq!(after.notified_through, 0);
        let mut invalid = notification;
        invalid.through_sequence = 0;
        assert!(store.has_pending_mailbox_inventory(invalid).await.is_err());
    }
}
