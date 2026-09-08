use super::super::LocalThreadStore;
use super::super::test_support::test_config;
use super::tests::create_paginated_thread;
use super::tests::turn_completed;
use super::tests::turn_started;
use crate::AcceptMailboxInputParams;
use crate::AppendThreadItemsParams;
use crate::MailboxPayload;
use crate::RevertThreadParams;
use crate::ThreadStore;
use crate::ThreadStoreError;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn revert_reconciles_inventory_without_blocking_unrecorded_preparations() {
    for evidence in ["absent", "recorded", "wrong-turn"] {
        let home = tempfile::tempdir().unwrap();
        let config = test_config(home.path());
        let state = codex_state::StateRuntime::init(
            config.sqlite.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let store = LocalThreadStore::new(config.clone(), Some(state.clone()));
        let receiver = ThreadId::new();
        create_paginated_thread(&store, receiver).await;
        let accepted = store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: "mail".to_string(),
                payload: MailboxPayload::User {
                    input: Vec::new(),
                    client_id: None,
                },
            })
            .await
            .unwrap();
        let notification = store
            .prepare_mailbox_inventory(receiver)
            .await
            .unwrap()
            .unwrap();
        let mut expected = store.read_mailbox_inventory(receiver).await.unwrap();
        let mut items = vec![
            turn_started("first"),
            turn_completed("first"),
            turn_started(&notification.id),
        ];
        if evidence != "absent" {
            let mut context = notification.context().unwrap();
            if evidence == "wrong-turn" {
                let ResponseItem::Message {
                    internal_chat_message_metadata_passthrough,
                    ..
                } = &mut context.item
                else {
                    panic!("expected developer inventory");
                };
                internal_chat_message_metadata_passthrough
                    .as_mut()
                    .unwrap()
                    .turn_id = Some("another-turn".to_string());
            }
            items.push(RolloutItem::ResponseItem(context));
        }
        items.push(turn_completed(&notification.id));
        store
            .append_items(AppendThreadItemsParams {
                thread_id: receiver,
                items,
            })
            .await
            .unwrap();
        let source = store.live_rollout_path(receiver).await.unwrap();
        store.shutdown_thread(receiver).await.unwrap();
        codex_rollout::state_db::reconcile_rollout(
            Some(state.as_ref()),
            source.as_path(),
            "test-provider",
            /*builder*/ None,
            &[],
            /*archived_only*/ Some(false),
            /*new_thread_memory_mode*/ None,
        )
        .await;
        let result = store
            .revert_thread(RevertThreadParams {
                thread_id: receiver,
                before_turn_id: notification.id.clone(),
                multi_agent_version: None,
            })
            .await;
        let current = state
            .get_thread(receiver)
            .await
            .unwrap()
            .unwrap()
            .rollout_path;
        if evidence == "wrong-turn" {
            assert!(matches!(result, Err(ThreadStoreError::Conflict { .. })));
            assert_eq!(current, source);
        } else {
            result.unwrap();
            assert_ne!(current, source);
            if evidence == "recorded" {
                expected.notified_through = notification.through_sequence;
                expected.active_notification = None;
            }
        }
        assert_eq!(
            store.read_mailbox_inventory(receiver).await.unwrap(),
            expected
        );
        assert_eq!(
            store.lookup_mailbox_input(receiver, "mail").await.unwrap(),
            Some(accepted)
        );
        drop(store);
        drop(state);
        let state = codex_state::StateRuntime::init(
            config.sqlite.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let reopened = LocalThreadStore::new(config, Some(state));
        assert_eq!(
            reopened.read_mailbox_inventory(receiver).await.unwrap(),
            expected
        );
    }
}
