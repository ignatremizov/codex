use super::super::LocalThreadStore;
use super::super::test_support::test_config;
use super::tests::compress_rollout;
use super::tests::create_paginated_thread;
use super::tests::turn_completed;
use super::tests::turn_started;
use crate::AcceptMailboxInputParams;
use crate::AppendThreadItemsParams;
use crate::ClaimMailboxInputParams;
use crate::MailboxInvocation;
use crate::MailboxMessageState;
use crate::MailboxPayload;
use crate::MailboxSelection;
use crate::RevertThreadParams;
use crate::ThreadStore;
use crate::ThreadStoreError;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mailbox_delivery_response_item_id;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_rollout::ResponseItemEnvelope;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy, Debug)]
enum Delivery {
    NotAppended,
    Complete,
    CompleteWithCopiedMetadata,
    ContextOnly,
    PresentationOnly,
    ConflictingContext,
    WrongPayload,
    WrongReceiver,
    WrongTurn,
}

#[tokio::test]
async fn recovery_does_not_treat_malformed_canonical_history_as_unrecorded_mail() {
    use std::io::Write;
    let home = tempfile::tempdir().unwrap();
    let config = test_config(home.path());
    let state_db = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = LocalThreadStore::new(config, Some(state_db.clone()));
    let receiver = ThreadId::new();
    create_paginated_thread(&store, receiver).await;
    store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "submission".to_string(),
            payload: MailboxPayload::User {
                input: Vec::new(),
                client_id: None,
            },
        })
        .await
        .unwrap();
    let params = ClaimMailboxInputParams {
        invocation: MailboxInvocation {
            receiver_thread_id: receiver,
            turn_id: "turn".to_string(),
            tool_call_id: "check".to_string(),
        },
        selection: MailboxSelection::All,
    };
    let expected = store.claim_mailbox_input(params.clone()).await.unwrap();
    let path = store.live_rollout_path(receiver).await.unwrap();
    store.shutdown_thread(receiver).await.unwrap();
    codex_rollout::state_db::reconcile_rollout(
        Some(state_db.as_ref()),
        path.as_path(),
        "test-provider",
        /*builder*/ None,
        &[],
        /*archived_only*/ Some(false),
        /*new_thread_memory_mode*/ None,
    )
    .await;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":\"response_item\",\"incomplete\":\n")
        .unwrap();
    let ThreadStoreError::Conflict { message } = store
        .recover_mailbox_delivery(params.clone())
        .await
        .unwrap_err()
    else {
        panic!("malformed history must stop recovery");
    };
    assert!(message.contains("not permission to resend"));
    assert_eq!(store.claim_mailbox_input(params).await.unwrap(), expected);
}

#[tokio::test]
async fn revert_reconciles_only_proven_delivery_and_preserves_ambiguous_recovery_state() {
    for delivery in [
        Delivery::NotAppended,
        Delivery::Complete,
        Delivery::CompleteWithCopiedMetadata,
        Delivery::ContextOnly,
        Delivery::PresentationOnly,
        Delivery::ConflictingContext,
        Delivery::WrongPayload,
        Delivery::WrongReceiver,
        Delivery::WrongTurn,
    ] {
        let home = tempfile::tempdir().unwrap();
        let config = test_config(home.path());
        let state_db = codex_state::StateRuntime::init(
            config.sqlite.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let store = LocalThreadStore::new(config.clone(), Some(state_db.clone()));
        let receiver = ThreadId::new();
        create_paginated_thread(&store, receiver).await;
        let input = vec![codex_protocol::user_input::UserInput::Image {
            image_url: "data:image/png;base64,b3JpZ2luYWw=".to_string(),
            detail: None,
        }];
        store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: "submission".to_string(),
                payload: MailboxPayload::User {
                    input: input.clone(),
                    client_id: Some("client".to_string()),
                },
            })
            .await
            .unwrap();
        let claim_params = ClaimMailboxInputParams {
            invocation: MailboxInvocation {
                receiver_thread_id: receiver,
                turn_id: "turn-2".to_string(),
                tool_call_id: "check-mail".to_string(),
            },
            selection: MailboxSelection::All,
        };
        let mut expected = store
            .claim_mailbox_input(claim_params.clone())
            .await
            .unwrap();
        let id = mailbox_delivery_response_item_id(&expected.messages[0].delivery_id).unwrap();
        let context = RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
            id: Some(id.clone()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "prepared image representation".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }));
        let mut completion = ItemCompletedEvent {
            thread_id: receiver,
            turn_id: claim_params.invocation.turn_id.clone(),
            item: TurnItem::UserMessage(UserMessageItem {
                id: id.to_string(),
                content: input,
                client_id: Some("client".to_string()),
            }),
            started_at_ms: None,
            completed_at_ms: 1,
        };
        match delivery {
            Delivery::WrongPayload => {
                let TurnItem::UserMessage(item) = &mut completion.item else {
                    panic!("expected user message");
                };
                item.client_id = Some("different-client".to_string());
            }
            Delivery::WrongReceiver => completion.thread_id = ThreadId::new(),
            Delivery::WrongTurn => completion.turn_id = "another-turn".to_string(),
            Delivery::NotAppended
            | Delivery::Complete
            | Delivery::CompleteWithCopiedMetadata
            | Delivery::ContextOnly
            | Delivery::PresentationOnly
            | Delivery::ConflictingContext => {}
        }
        let mut items = vec![
            turn_started("turn-1"),
            turn_completed("turn-1"),
            turn_started("turn-2"),
        ];
        if !matches!(delivery, Delivery::NotAppended | Delivery::PresentationOnly) {
            items.push(context.clone());
        }
        if !matches!(delivery, Delivery::NotAppended | Delivery::ContextOnly) {
            items.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(completion)));
        }
        if matches!(delivery, Delivery::ConflictingContext) {
            let RolloutItem::ResponseItem(mut conflicting) = context else {
                panic!("expected model envelope");
            };
            let ResponseItem::Message { content, .. } = &mut conflicting.item else {
                panic!("expected user context");
            };
            content.clear();
            items.push(RolloutItem::ResponseItem(conflicting));
        }
        items.push(turn_completed("turn-2"));
        store
            .append_items(AppendThreadItemsParams {
                thread_id: receiver,
                items,
            })
            .await
            .unwrap();
        let original_path = store.live_rollout_path(receiver).await.unwrap();
        store.shutdown_thread(receiver).await.unwrap();
        codex_rollout::state_db::reconcile_rollout(
            Some(state_db.as_ref()),
            original_path.as_path(),
            "test-provider",
            /*builder*/ None,
            &[],
            /*archived_only*/ Some(false),
            /*new_thread_memory_mode*/ None,
        )
        .await;
        let original_history = codex_rollout::RolloutRecorder::load_rollout_items(&original_path)
            .await
            .unwrap();
        if matches!(delivery, Delivery::CompleteWithCopiedMetadata) {
            use std::io::Write;
            let mut copied = serde_json::to_value(&original_history.0[0]).unwrap();
            let source = ThreadId::new();
            copied["timestamp"] = serde_json::json!("2025-01-03T12:00:01Z");
            copied["payload"]["id"] = serde_json::json!(source);
            copied["payload"]["session_id"] = serde_json::json!(source);
            copied["payload"]["history_mode"] = serde_json::json!("future");
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&original_path)
                    .unwrap(),
                "{copied}"
            )
            .unwrap();
            // The ordinary reader still reports the skipped record. Recovery
            // proves it unrelated without changing artifacts or consuming mail.
            assert_eq!(
                codex_rollout::RolloutRecorder::load_rollout_items(&original_path)
                    .await
                    .unwrap()
                    .2,
                1
            );
            let recovered = store
                .recover_mailbox_delivery(claim_params.clone())
                .await
                .unwrap();
            assert_eq!(recovered.claim, expected);
            assert!(matches!(
                recovered.deliveries[0].artifacts,
                crate::MailboxDeliveryArtifacts::Complete { .. }
            ));
        }
        if matches!(
            delivery,
            Delivery::Complete | Delivery::CompleteWithCopiedMetadata
        ) {
            compress_rollout(&original_path);
        }
        let result = store
            .revert_thread(RevertThreadParams {
                thread_id: receiver,
                before_turn_id: "turn-2".to_string(),
                multi_agent_version: None,
            })
            .await;
        let current_path = state_db
            .get_thread(receiver)
            .await
            .unwrap()
            .unwrap()
            .rollout_path;
        match delivery {
            Delivery::NotAppended | Delivery::Complete | Delivery::CompleteWithCopiedMetadata => {
                result.unwrap();
                assert_ne!(current_path, original_path, "{delivery:?}");
                if matches!(
                    delivery,
                    Delivery::Complete | Delivery::CompleteWithCopiedMetadata
                ) {
                    expected.messages[0].message.state = MailboxMessageState::Consumed;
                }
            }
            Delivery::ContextOnly
            | Delivery::PresentationOnly
            | Delivery::ConflictingContext
            | Delivery::WrongPayload
            | Delivery::WrongReceiver
            | Delivery::WrongTurn => {
                let ThreadStoreError::Conflict { message } = result.unwrap_err() else {
                    panic!("expected recoverable conflict for {delivery:?}");
                };
                assert!(message.contains("mailbox recovery required"), "{message}");
                assert!(message.contains("not permission to resend"), "{message}");
                assert_eq!(current_path, original_path);
                assert_eq!(
                    serde_json::to_value(
                        codex_rollout::RolloutRecorder::load_rollout_items(&original_path)
                            .await
                            .unwrap()
                    )
                    .unwrap(),
                    serde_json::to_value(original_history).unwrap()
                );
            }
        }
        // Reopen the adapter: neither a switched rollout nor an interrupted
        // delivery may erase fixed membership, delivery IDs, or terminal state.
        drop(store);
        drop(state_db);
        let reopened_db = codex_state::StateRuntime::init(
            config.sqlite.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let reopened = LocalThreadStore::new(config, Some(reopened_db));
        assert_eq!(
            reopened
                .claim_mailbox_input(claim_params.clone())
                .await
                .unwrap(),
            expected,
            "{delivery:?}"
        );
        if matches!(
            delivery,
            Delivery::Complete | Delivery::CompleteWithCopiedMetadata
        ) {
            reopened
                .revert_thread(RevertThreadParams {
                    thread_id: receiver,
                    before_turn_id: "turn-1".to_string(),
                    multi_agent_version: None,
                })
                .await
                .unwrap();
            assert_eq!(
                reopened.claim_mailbox_input(claim_params).await.unwrap(),
                expected
            );
        }
    }
}
