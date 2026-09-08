use super::*;
use crate::ClaimedMailboxInput;
use crate::MailboxInvocation;
use crate::MailboxSelection;
use crate::MailboxSender;
use crate::StoredMailboxInput;
use codex_protocol::ThreadId;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::ResponseItemEnvelope;
use pretty_assertions::assert_eq;

fn delivery() -> (MailboxClaim, MailboxDeliveryEvidence, Vec<RolloutItem>) {
    let receiver = ThreadId::new();
    let delivery_id = ThreadId::new().to_string();
    let response_id = ResponseItemId::with_suffix("msg_mailbox", &delivery_id);
    let input = vec![UserInput::Image {
        image_url: "data:image/png;base64,b3JpZ2luYWw=".to_string(),
        detail: None,
    }];
    let member = ClaimedMailboxInput {
        message: StoredMailboxInput {
            id: ThreadId::new().to_string(),
            receiver_thread_id: receiver,
            submission_key: "submission".to_string(),
            sender: MailboxSender::User,
            payload: MailboxPayload::User {
                input: input.clone(),
                client_id: Some("client-id".to_string()),
            },
            acceptance_sequence: 1,
            state: MailboxMessageState::Claimed,
            rejection_reason: None,
        },
        delivery_id,
    };
    // Prepared context deliberately differs from original typed attachment data.
    let mut context = ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(response_id.clone()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "prepared media context".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    context.item.set_turn_id_if_missing("receiver-turn");
    context
        .item
        .set_create_time_if_missing(serde_json::Number::from(123456));
    let evidence = MailboxDeliveryEvidence {
        message_id: member.message.id.clone(),
        prepared_context: context.clone(),
    };
    let claim = MailboxClaim {
        invocation: MailboxInvocation {
            receiver_thread_id: receiver,
            turn_id: "receiver-turn".to_string(),
            tool_call_id: "check-mail".to_string(),
        },
        selection: MailboxSelection::All,
        messages: vec![member],
    };
    let history = vec![
        RolloutItem::ResponseItem(context),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: receiver,
            turn_id: claim.invocation.turn_id.clone(),
            item: TurnItem::UserMessage(UserMessageItem {
                id: response_id.to_string(),
                client_id: Some("client-id".to_string()),
                content: input,
            }),
            started_at_ms: None,
            completed_at_ms: 1,
        })),
    ];
    (claim, evidence, history)
}

#[test]
fn historical_delivery_requires_both_prepared_context_and_original_presentation() {
    let (claim, evidence, history) = delivery();
    let expected = vec![(
        claim.messages[0].message.id.clone(),
        claim.messages[0].delivery_id.clone(),
    )];
    assert_eq!(
        verified_deliveries(&claim, std::slice::from_ref(&evidence), &history).unwrap(),
        expected
    );
    for incomplete in [&history[..1], &history[1..], &[]] {
        assert_eq!(
            verified_deliveries(&claim, std::slice::from_ref(&evidence), incomplete).unwrap(),
            Vec::new()
        );
    }
}

#[test]
fn evidence_must_be_unique_claim_bound_and_use_reserved_user_message_identity() {
    let (claim, evidence, history) = delivery();
    assert!(verified_deliveries(&claim, &[evidence.clone(), evidence.clone()], &history).is_err());
    let mut outside = evidence.clone();
    outside.message_id = "another-message".to_string();
    assert!(verified_deliveries(&claim, &[outside], &history).is_err());
    let mut wrong_id = evidence.clone();
    wrong_id.prepared_context.item = ResponseItem::Message {
        id: Some(ResponseItemId::new("msg")),
        role: "user".to_string(),
        content: Vec::new(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(verified_deliveries(&claim, &[wrong_id], &history).is_err());
    let mut wrong_role = evidence.clone();
    if let ResponseItem::Message { role, .. } = &mut wrong_role.prepared_context.item {
        *role = "assistant".to_string();
    }
    assert!(verified_deliveries(&claim, &[wrong_role], &history).is_err());
    let mut wrong_type = evidence;
    wrong_type.prepared_context.item = ResponseItem::Compaction {
        id: None,
        encrypted_content: "not a message".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(verified_deliveries(&claim, &[wrong_type], &history).is_err());
}

#[test]
fn foreign_receiver_wrong_turn_and_changed_original_payload_are_not_delivery_proof() {
    let (claim, evidence, history) = delivery();
    for changed in 0..4 {
        let mut history = history.clone();
        if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = &mut history[1] {
            match changed {
                0 => event.thread_id = ThreadId::new(),
                1 => event.turn_id = "other-turn".to_string(),
                2 => {
                    if let TurnItem::UserMessage(message) = &mut event.item {
                        message.content.clear();
                    }
                }
                3 => {
                    if let TurnItem::UserMessage(message) = &mut event.item {
                        message.client_id = None;
                    }
                }
                _ => unreachable!(),
            }
        }
        assert!(matches!(
            verified_deliveries(&claim, std::slice::from_ref(&evidence), &history),
            Err(ThreadStoreError::Conflict { .. })
        ));
    }
}

#[test]
fn terminal_members_do_not_generate_another_acknowledgement() {
    let (mut claim, evidence, history) = delivery();
    for state in [MailboxMessageState::Consumed, MailboxMessageState::Rejected] {
        claim.messages[0].message.state = state;
        assert_eq!(
            verified_deliveries(&claim, std::slice::from_ref(&evidence), &history).unwrap(),
            Vec::new()
        );
    }
}

#[test]
fn rollback_and_compaction_do_not_erase_historical_append_proof() {
    let (claim, evidence, mut history) = delivery();
    history.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        codex_protocol::protocol::ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: Some(1),
            rollback_start_index: Some(0),
        },
    )));
    history.push(RolloutItem::Compacted(codex_rollout::CompactedItem {
        message: "compacted away".to_string(),
        replacement_history: Some(Vec::new()),
        ..Default::default()
    }));
    assert_eq!(
        verified_deliveries(&claim, &[evidence], &history).unwrap(),
        vec![(
            claim.messages[0].message.id.clone(),
            claim.messages[0].delivery_id.clone()
        )]
    );
}

#[test]
fn recovery_returns_exact_artifacts_for_each_partial_and_complete_state() {
    let (claim, _, history) = delivery();
    for (items, expected_kind) in [
        (&history[..0], "none"),
        (&history[..1], "context"),
        (&history[1..], "presentation"),
        (&history[..], "complete"),
    ] {
        let mut recovered = recover_claims(std::slice::from_ref(&claim), items).unwrap();
        let recovered = recovered.pop().unwrap();
        assert_eq!(recovered.claim, claim);
        assert_eq!(recovered.deliveries.len(), 1);
        assert_eq!(
            recovered.deliveries[0].message_id,
            claim.messages[0].message.id
        );
        let mut returned = Vec::new();
        let kind = match &recovered.deliveries[0].artifacts {
            MailboxDeliveryArtifacts::NotRecorded => "none",
            MailboxDeliveryArtifacts::ContextOnly { prepared_context } => {
                returned.push(RolloutItem::ResponseItem(prepared_context.clone()));
                "context"
            }
            MailboxDeliveryArtifacts::PresentationOnly { completion } => {
                returned.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                    completion.clone(),
                )));
                "presentation"
            }
            MailboxDeliveryArtifacts::Complete {
                prepared_context,
                completion,
            } => {
                returned.push(RolloutItem::ResponseItem(prepared_context.clone()));
                returned.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                    completion.clone(),
                )));
                "complete"
            }
        };
        assert_eq!(kind, expected_kind);
        assert_eq!(
            serde_json::to_value(returned).unwrap(),
            serde_json::to_value(items).unwrap()
        );
    }
}

#[test]
fn recovery_accepts_identical_retries_but_rejects_conflicting_artifacts() {
    let (claim, _, history) = delivery();
    let duplicate = [history.clone(), history.clone()].concat();
    let recovered = recover_claims(std::slice::from_ref(&claim), &duplicate).unwrap();
    assert!(matches!(
        recovered[0].deliveries[0].artifacts,
        MailboxDeliveryArtifacts::Complete { .. }
    ));
    for index in 0..2 {
        let mut conflicting = history.clone();
        let mut changed = history[index].clone();
        match &mut changed {
            RolloutItem::ResponseItem(envelope) => {
                let ResponseItem::Message { content, .. } = &mut envelope.item else {
                    panic!("expected context");
                };
                content.clear();
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => event.completed_at_ms += 1,
            _ => panic!("expected artifact"),
        }
        conflicting.push(changed);
        assert!(matches!(
            recover_claims(std::slice::from_ref(&claim), &conflicting),
            Err(ThreadStoreError::Conflict { .. })
        ));
    }
}

#[test]
fn batch_recovery_keeps_claim_order_and_terminal_state_authoritative() {
    let (first, _, first_history) = delivery();
    let (mut terminal, _, terminal_history) = delivery();
    terminal.messages[0].message.state = MailboxMessageState::Rejected;
    let claims = vec![terminal, first];
    let history = [first_history, terminal_history].concat();
    let recovered = recover_claims(&claims, &history).unwrap();
    assert_eq!(
        recovered.iter().map(|item| &item.claim).collect::<Vec<_>>(),
        claims.iter().collect::<Vec<_>>()
    );
    assert!(matches!(
        recovered[0].deliveries[0].artifacts,
        MailboxDeliveryArtifacts::NotRecorded
    ));
    assert!(matches!(
        recovered[1].deliveries[0].artifacts,
        MailboxDeliveryArtifacts::Complete { .. }
    ));
}
