use super::*;
use crate::InMemoryThreadStore;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::ThreadStore;
use codex_protocol::AgentInputIdentity;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[path = "mailbox_inventory_tests.rs"]
mod inventory;

#[path = "mailbox_read_tests.rs"]
mod reads;

async fn stores(
    home: &tempfile::TempDir,
) -> (Vec<Arc<dyn ThreadStore>>, Arc<codex_state::StateRuntime>) {
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
    (
        vec![
            Arc::new(InMemoryThreadStore::default()),
            Arc::new(LocalThreadStore::new(config, Some(runtime.clone()))),
        ],
        runtime,
    )
}

fn user_submission(receiver_thread_id: ThreadId, key: &str) -> AcceptMailboxInputParams {
    AcceptMailboxInputParams {
        receiver_thread_id,
        submission_key: key.to_string(),
        payload: MailboxPayload::User {
            input: vec![
                UserInput::Text {
                    text: format!("Message {key}"),
                    text_elements: Vec::new(),
                },
                UserInput::Image {
                    image_url: "data:image/png;base64,cGF5bG9hZA==".to_string(),
                    detail: None,
                },
            ],
            client_id: Some(format!("client-{key}")),
        },
    }
}

fn identity(thread_id: ThreadId) -> AgentInputIdentity {
    AgentInputIdentity {
        thread_id,
        nickname: Some("snapshot-name".to_string()),
        agent_ref: Some("2".to_string()),
        task_path: Some("/root/research".to_string()),
        role: Some("researcher".to_string()),
        model: None,
        reasoning_effort: None,
    }
}

fn claim_params(receiver_thread_id: ThreadId, call: &str) -> ClaimMailboxInputParams {
    ClaimMailboxInputParams {
        invocation: MailboxInvocation {
            receiver_thread_id,
            turn_id: "receiver-turn".to_string(),
            tool_call_id: call.to_string(),
        },
        selection: MailboxSelection::All,
    }
}

#[tokio::test]
async fn typed_acceptance_is_immutable_receiver_scoped_and_preserves_authorship() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _runtime) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let params = user_submission(receiver, "same-key");
        let accepted = store.accept_mailbox_input(params.clone()).await.unwrap();
        assert_eq!(
            accepted,
            StoredMailboxInput {
                id: accepted.id.clone(),
                receiver_thread_id: receiver,
                submission_key: params.submission_key.clone(),
                sender: MailboxSender::User,
                payload: params.payload.clone(),
                acceptance_sequence: accepted.acceptance_sequence,
                state: MailboxMessageState::Pending,
                rejection_reason: None,
            }
        );
        assert_eq!(
            store.accept_mailbox_input(params.clone()).await.unwrap(),
            accepted
        );
        let mut conflicting = params.clone();
        conflicting.payload = MailboxPayload::User {
            input: Vec::new(),
            client_id: None,
        };
        assert!(matches!(
            store.accept_mailbox_input(conflicting).await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));
        let mut other_receiver = params;
        other_receiver.receiver_thread_id = ThreadId::new();
        let other = store.accept_mailbox_input(other_receiver).await.unwrap();
        assert_ne!(other.id, accepted.id);

        let sender = ThreadId::new();
        let agent_payload = MailboxPayload::Agent {
            input: vec![UserInput::Audio {
                audio_url: "data:audio/wav;base64,YXVkaW8=".to_string(),
            }],
            attribution: Box::new(AgentInputAttribution {
                sender: identity(sender),
                recipient: identity(receiver),
                sender_turn_id: "sender-turn".to_string(),
            }),
        };
        let agent = store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: "agent".to_string(),
                payload: agent_payload.clone(),
            })
            .await
            .unwrap();
        assert_eq!(
            (agent.sender, agent.payload),
            (MailboxSender::Agent(sender), agent_payload.clone())
        );
        assert!(matches!(
            store
                .accept_mailbox_input(AcceptMailboxInputParams {
                    receiver_thread_id: ThreadId::new(),
                    submission_key: "wrong-recipient".to_string(),
                    payload: agent_payload,
                })
                .await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));
    }
}

#[tokio::test]
async fn claims_keep_fixed_membership_empty_batches_and_canonical_sender_sets() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _runtime) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let empty_params = claim_params(receiver, "empty");
        let empty = store
            .claim_mailbox_input(empty_params.clone())
            .await
            .unwrap();
        assert_eq!(
            empty,
            MailboxClaim {
                invocation: empty_params.invocation.clone(),
                selection: MailboxSelection::All,
                messages: Vec::new(),
            }
        );
        let mut first = store
            .accept_mailbox_input(user_submission(receiver, "first"))
            .await
            .unwrap();
        assert_eq!(
            store.claim_mailbox_input(empty_params).await.unwrap(),
            empty
        );
        let mut params = claim_params(receiver, "selected");
        params.selection =
            MailboxSelection::Senders(vec![MailboxSender::User, MailboxSender::User]);
        let selected = store.claim_mailbox_input(params.clone()).await.unwrap();
        first.state = MailboxMessageState::Claimed;
        assert_eq!(
            selected,
            MailboxClaim {
                invocation: params.invocation.clone(),
                selection: MailboxSelection::Senders(vec![MailboxSender::User]),
                messages: vec![ClaimedMailboxInput {
                    message: first,
                    delivery_id: selected.messages[0].delivery_id.clone(),
                }],
            }
        );
        store
            .accept_mailbox_input(user_submission(receiver, "later"))
            .await
            .unwrap();
        params.selection = MailboxSelection::Senders(vec![MailboxSender::User]);
        assert_eq!(
            store.claim_mailbox_input(params.clone()).await.unwrap(),
            selected
        );
        params.selection = MailboxSelection::All;
        assert!(matches!(
            store.claim_mailbox_input(params).await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));

        let (left, right) = tokio::join!(
            store.claim_mailbox_input(claim_params(receiver, "left")),
            store.claim_mailbox_input(claim_params(receiver, "right")),
        );
        let left = left.unwrap();
        let right = right.unwrap();
        assert_eq!(left.messages.len() + right.messages.len(), 1);
        assert!(left.messages.iter().all(|member| {
            right
                .messages
                .iter()
                .all(|other| member.message.id != other.message.id)
        }));
    }
}

#[tokio::test]
async fn local_claim_retries_report_terminal_state_from_sql_without_replacing_ids() {
    let home = tempfile::tempdir().unwrap();
    let (stores, runtime) = stores(&home).await;
    let local = &stores[1];
    let receiver = ThreadId::new();
    local
        .accept_mailbox_input(user_submission(receiver, "reject"))
        .await
        .unwrap();
    let params = claim_params(receiver, "claim");
    let mut expected = local.claim_mailbox_input(params.clone()).await.unwrap();
    let member = &mut expected.messages[0];
    runtime
        .thread_queue()
        .reject_mail(receiver, &member.message.id, "permission revoked")
        .await
        .unwrap();
    member.message.state = MailboxMessageState::Rejected;
    member.message.rejection_reason = Some("permission revoked".to_string());
    assert_eq!(local.claim_mailbox_input(params).await.unwrap(), expected);
}

#[tokio::test]
async fn invalid_keys_and_invocations_do_not_reserve_mail() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _runtime) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        assert!(matches!(
            store
                .accept_mailbox_input(user_submission(receiver, ""))
                .await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));
        let mut params = claim_params(receiver, "valid");
        params.invocation.turn_id.clear();
        assert!(matches!(
            store.claim_mailbox_input(params).await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));
        let mut params = claim_params(receiver, "valid");
        params.invocation.tool_call_id.clear();
        assert!(matches!(
            store.claim_mailbox_input(params).await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));
    }
}

#[tokio::test]
async fn sender_selection_uses_uuid_and_retains_acceptance_order_without_consuming_user_mail() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _runtime) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let user = store
            .accept_mailbox_input(user_submission(receiver, "user"))
            .await
            .unwrap();
        let first_sender = ThreadId::new();
        let second_sender = ThreadId::new();
        let mut accepted = Vec::new();
        for (index, sender) in [first_sender, second_sender, first_sender]
            .into_iter()
            .enumerate()
        {
            let message = store
                .accept_mailbox_input(AcceptMailboxInputParams {
                    receiver_thread_id: receiver,
                    submission_key: format!("agent-{index}"),
                    payload: MailboxPayload::Agent {
                        input: vec![UserInput::Text {
                            text: format!("payload-{index}"),
                            text_elements: Vec::new(),
                        }],
                        attribution: Box::new(AgentInputAttribution {
                            // Both agents deliberately have identical display labels.
                            sender: identity(sender),
                            recipient: identity(receiver),
                            sender_turn_id: "sender-turn".to_string(),
                        }),
                    },
                })
                .await
                .unwrap();
            accepted.push(message.id);
        }
        let mut params = claim_params(receiver, "agents");
        params.selection = MailboxSelection::Senders(vec![
            MailboxSender::Agent(second_sender),
            MailboxSender::Agent(first_sender),
            MailboxSender::Agent(second_sender),
        ]);
        let selected = store.claim_mailbox_input(params).await.unwrap();
        assert_eq!(
            selected
                .messages
                .into_iter()
                .map(|member| member.message.id)
                .collect::<Vec<_>>(),
            accepted
        );
        let remaining = store
            .claim_mailbox_input(claim_params(receiver, "user"))
            .await
            .unwrap();
        assert_eq!(
            remaining
                .messages
                .into_iter()
                .map(|member| member.message.id)
                .collect::<Vec<_>>(),
            vec![user.id]
        );
    }
}

#[tokio::test]
async fn reconciliation_requires_durable_context_and_typed_agent_presentation() {
    use crate::AppendThreadItemsParams;
    use crate::CreateThreadParams;
    use crate::ThreadPersistenceMetadata;
    use codex_protocol::ResponseItemId;
    use codex_protocol::items::AgentMessageItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::ThreadHistoryMode;
    use codex_protocol::protocol::ThreadMemoryMode;
    use codex_protocol::protocol::ThreadRolledBackEvent;
    use codex_rollout::RolloutItem;

    let home = tempfile::tempdir().unwrap();
    let (stores, _runtime) = stores(&home).await;
    for (store, history_mode) in stores.iter().flat_map(|store| {
        [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated]
            .map(|history_mode| (store, history_mode))
    }) {
        let receiver = ThreadId::new();
        store
            .create_thread(CreateThreadParams {
                session_id: receiver.into(),
                thread_id: receiver,
                extra_config: None,
                forked_from_id: None,
                parent_thread_id: None,
                source: SessionSource::Exec,
                thread_source: None,
                originator: "test".to_string(),
                base_instructions: BaseInstructions::default(),
                dynamic_tools: Vec::new(),
                selected_capability_roots: Vec::new(),
                multi_agent_version: None,
                history_mode,
                history_base: None,
                subagent_history_start_ordinal: None,
                initial_window_id: ThreadId::new().to_string(),
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(home.path().to_path_buf()),
                    model_provider: "test-provider".to_string(),
                    memory_mode: ThreadMemoryMode::Enabled,
                },
            })
            .await
            .unwrap();
        let attribution = AgentInputAttribution {
            sender: identity(ThreadId::new()),
            recipient: identity(receiver),
            sender_turn_id: "sender-turn".to_string(),
        };
        let input = vec![UserInput::Image {
            image_url: "data:image/png;base64,b3JpZ2luYWw=".to_string(),
            detail: None,
        }];
        store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: "agent-mail".to_string(),
                payload: MailboxPayload::Agent {
                    input: input.clone(),
                    attribution: Box::new(attribution.clone()),
                },
            })
            .await
            .unwrap();
        let params = claim_params(receiver, "delivery");
        let inventory_notification = store
            .prepare_mailbox_inventory(receiver)
            .await
            .unwrap()
            .unwrap();
        let mut expected = store.claim_mailbox_input(params.clone()).await.unwrap();
        store
            .accept_mailbox_input(AcceptMailboxInputParams {
                receiver_thread_id: receiver,
                submission_key: "later-agent-mail".to_string(),
                payload: MailboxPayload::Agent {
                    input: input.clone(),
                    attribution: Box::new(attribution.clone()),
                },
            })
            .await
            .unwrap();
        let member = &expected.messages[0];
        let response_id = ResponseItemId::with_suffix("msg_mailbox", &member.delivery_id);
        let prepared_context = ResponseItemEnvelope::new(ResponseItem::Message {
            id: Some(response_id.clone()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "prepared attachment representation".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        });
        let reconcile = ReconcileMailboxDeliveryParams {
            claim: params,
            deliveries: vec![MailboxDeliveryEvidence {
                message_id: member.message.id.clone(),
                prepared_context: prepared_context.clone(),
            }],
        };
        store
            .append_items_and_flush(AppendThreadItemsParams {
                thread_id: receiver,
                items: vec![RolloutItem::ResponseItem(prepared_context)],
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .reconcile_mailbox_delivery(reconcile.clone())
                .await
                .unwrap(),
            expected
        );
        let recovered = store
            .recover_mailbox_delivery(reconcile.claim.clone())
            .await
            .unwrap();
        assert_eq!(recovered.claim, expected);
        let [
            RecoveredMailboxDelivery {
                artifacts: MailboxDeliveryArtifacts::ContextOnly { prepared_context },
                ..
            },
        ] = recovered.deliveries.as_slice()
        else {
            panic!("expected context-only recovery");
        };
        assert_eq!(prepared_context, &reconcile.deliveries[0].prepared_context);
        store
            .append_items_and_flush(AppendThreadItemsParams {
                thread_id: receiver,
                items: vec![
                    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id: receiver,
                        turn_id: reconcile.claim.invocation.turn_id.clone(),
                        item: TurnItem::AgentMessage(AgentMessageItem {
                            id: response_id.to_string(),
                            content: Vec::new(),
                            attribution: Some(attribution),
                            input: Some(input),
                            phase: None,
                            memory_citation: None,
                            delivery: None,
                            questions: None,
                            sub_agent_completion: None,
                        }),
                        started_at_ms: None,
                        completed_at_ms: 1,
                    })),
                    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                        num_turns: 1,
                        materialized_turns: Some(1),
                        rollback_start_index: Some(1),
                    })),
                ],
            })
            .await
            .unwrap();
        let recovered = store
            .recover_mailbox_delivery(reconcile.claim.clone())
            .await
            .unwrap();
        assert_eq!(recovered.claim, expected);
        let [
            RecoveredMailboxDelivery {
                artifacts:
                    MailboxDeliveryArtifacts::Complete {
                        prepared_context,
                        completion,
                    },
                ..
            },
        ] = recovered.deliveries.as_slice()
        else {
            panic!("expected complete recovery");
        };
        assert_eq!(prepared_context, &reconcile.deliveries[0].prepared_context);
        assert_eq!(completion.turn_id, reconcile.claim.invocation.turn_id);
        expected.messages[0].message.state = MailboxMessageState::Consumed;
        assert_eq!(
            store
                .reconcile_mailbox_delivery(reconcile.clone())
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            store
                .lookup_mailbox_claim(expected.invocation.clone())
                .await
                .unwrap(),
            Some(expected.clone())
        );
        assert!(
            !store
                .has_pending_mailbox_inventory(inventory_notification)
                .await
                .unwrap()
        );
        assert_eq!(
            store.reconcile_mailbox_delivery(reconcile).await.unwrap(),
            expected
        );
        assert!(
            store
                .reject_mailbox_input(RejectMailboxInputParams {
                    receiver_thread_id: receiver,
                    message_id: expected.messages[0].message.id.clone(),
                    reason: "too late".to_string(),
                })
                .await
                .is_err()
        );
        store.shutdown_thread(receiver).await.unwrap();
        assert_eq!(
            store
                .lookup_mailbox_input(receiver, "agent-mail")
                .await
                .unwrap(),
            Some(expected.messages[0].message.clone())
        );
    }
}

#[tokio::test]
async fn recovery_establishes_an_empty_fixed_claim_and_does_not_admit_later_mail() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let params = claim_params(receiver, "empty-recovery");
        let first = store
            .recover_mailbox_delivery(params.clone())
            .await
            .unwrap();
        assert!(first.claim.messages.is_empty());
        assert!(first.deliveries.is_empty());
        store
            .accept_mailbox_input(user_submission(receiver, "later"))
            .await
            .unwrap();
        let retry = store.recover_mailbox_delivery(params).await.unwrap();
        assert_eq!(retry.claim, first.claim);
        assert!(retry.deliveries.is_empty());
        assert_eq!(
            store
                .claim_mailbox_input(claim_params(receiver, "next"))
                .await
                .unwrap()
                .messages
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn typed_rejection_preserves_content_membership_and_terminal_outcome() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        for claimed in [false, true] {
            let receiver = ThreadId::new();
            let accepted = store
                .accept_mailbox_input(user_submission(receiver, "reject"))
                .await
                .unwrap();
            let claim = claim_params(receiver, "check");
            let original = if claimed {
                Some(store.claim_mailbox_input(claim.clone()).await.unwrap())
            } else {
                None
            };
            let params = RejectMailboxInputParams {
                receiver_thread_id: receiver,
                message_id: accepted.id.clone(),
                reason: "permission revoked".to_string(),
            };
            let mut wrong_receiver = params.clone();
            wrong_receiver.receiver_thread_id = ThreadId::new();
            assert!(store.reject_mailbox_input(wrong_receiver).await.is_err());
            let mut empty_reason = params.clone();
            empty_reason.reason.clear();
            assert!(store.reject_mailbox_input(empty_reason).await.is_err());
            let mut expected = accepted;
            expected.state = MailboxMessageState::Rejected;
            expected.rejection_reason = Some(params.reason.clone());
            assert_eq!(
                store.reject_mailbox_input(params.clone()).await.unwrap(),
                expected
            );
            assert_eq!(
                store.reject_mailbox_input(params.clone()).await.unwrap(),
                expected
            );
            let mut changed_reason = params;
            changed_reason.reason = "another reason".to_string();
            assert!(store.reject_mailbox_input(changed_reason).await.is_err());
            let recovered = store.recover_mailbox_delivery(claim).await.unwrap();
            if let Some(mut original) = original {
                original.messages[0].message = expected;
                assert_eq!(recovered.claim, original);
                assert!(matches!(
                    recovered.deliveries[0].artifacts,
                    MailboxDeliveryArtifacts::NotRecorded
                ));
            } else {
                assert!(recovered.claim.messages.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn submission_lookup_preserves_frozen_attribution_and_returns_current_state() {
    let home = tempfile::tempdir().unwrap();
    let (stores, _) = stores(&home).await;
    for store in stores {
        let receiver = ThreadId::new();
        let key = "original-call";
        assert_eq!(
            store.lookup_mailbox_input(receiver, key).await.unwrap(),
            None
        );
        let params = AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: key.to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Image {
                    image_url: "data:image/png;base64,b3JpZ2luYWw=".to_string(),
                    detail: None,
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(ThreadId::new()),
                    recipient: identity(receiver),
                    sender_turn_id: "original-sender-turn".to_string(),
                }),
            },
        };
        let mut expected = store.accept_mailbox_input(params.clone()).await.unwrap();
        let mut changed_snapshot = params.clone();
        let MailboxPayload::Agent { attribution, .. } = &mut changed_snapshot.payload else {
            panic!("expected agent input");
        };
        attribution.sender.nickname = Some("renamed".to_string());
        attribution.sender.model = Some("different-model".to_string());
        attribution.recipient.nickname = Some("renamed-receiver".to_string());
        assert!(store.accept_mailbox_input(changed_snapshot).await.is_err());
        let existing = store
            .lookup_mailbox_input(receiver, key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(existing, expected);
        // Caller verification of sender/turn/original input precedes this reuse.
        // Lookup does not relax immutable acceptance or recapture display metadata.
        let retry = AcceptMailboxInputParams {
            payload: existing.payload,
            ..params
        };
        assert_eq!(store.accept_mailbox_input(retry).await.unwrap(), expected);
        assert_eq!(
            store
                .lookup_mailbox_input(receiver, "other-call")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store.lookup_mailbox_input(receiver, "").await.unwrap(),
            None
        );
        let other_receiver = ThreadId::new();
        assert_eq!(
            store
                .lookup_mailbox_input(other_receiver, key)
                .await
                .unwrap(),
            None
        );
        let other = store
            .accept_mailbox_input(user_submission(other_receiver, key))
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_mailbox_input(other_receiver, key)
                .await
                .unwrap(),
            Some(other)
        );
        assert_eq!(
            store.lookup_mailbox_input(receiver, key).await.unwrap(),
            Some(expected.clone())
        );

        let claim = store
            .claim_mailbox_input(claim_params(receiver, "check"))
            .await
            .unwrap();
        expected.state = MailboxMessageState::Claimed;
        assert_eq!(claim.messages[0].message, expected);
        assert_eq!(
            store.lookup_mailbox_input(receiver, key).await.unwrap(),
            Some(expected.clone())
        );
        store
            .reject_mailbox_input(RejectMailboxInputParams {
                receiver_thread_id: receiver,
                message_id: expected.id.clone(),
                reason: "revoked".to_string(),
            })
            .await
            .unwrap();
        expected.state = MailboxMessageState::Rejected;
        expected.rejection_reason = Some("revoked".to_string());
        assert_eq!(
            store.lookup_mailbox_input(receiver, key).await.unwrap(),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn local_submission_lookup_survives_adapter_reopening() {
    let home = tempfile::tempdir().unwrap();
    let (original_stores, runtime) = stores(&home).await;
    let receiver = ThreadId::new();
    let expected = original_stores[1]
        .accept_mailbox_input(user_submission(receiver, "durable-submission"))
        .await
        .unwrap();
    drop(original_stores);
    drop(runtime);
    let (reopened_stores, _runtime) = stores(&home).await;
    assert_eq!(
        reopened_stores[1]
            .lookup_mailbox_input(receiver, "durable-submission")
            .await
            .unwrap(),
        Some(expected)
    );
}
