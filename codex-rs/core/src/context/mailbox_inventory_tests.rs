use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use test_case::test_case;

fn notification(receiver: ThreadId, senders: Vec<MailboxSender>) -> MailboxInventoryNotification {
    let mut pending_senders = senders
        .into_iter()
        .enumerate()
        .map(|(index, sender)| MailboxSenderInventory {
            sender,
            count: index as u64 + 1,
            max_acceptance_sequence: index as i64 + 1,
        })
        .collect::<Vec<_>>();
    pending_senders.sort_by_key(|group| match group.sender {
        MailboxSender::User => "user".to_string(),
        MailboxSender::Agent(id) => format!("agent:{id}"),
    });
    MailboxInventoryNotification {
        id: uuid::Uuid::now_v7().to_string(),
        receiver_thread_id: receiver,
        through_sequence: pending_senders.len() as i64,
        pending_senders,
    }
}

fn text(item: &ResponseItem) -> &str {
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected single text");
    };
    text
}

#[test]
fn projects_frozen_counts_and_only_advertised_refs_without_mutating_canonical_metadata() {
    let receiver = ThreadId::new();
    let advertised = ThreadId::new();
    let unadvertised = ThreadId::new();
    let snapshot = notification(
        receiver,
        vec![
            MailboxSender::Agent(advertised),
            MailboxSender::Agent(unadvertised),
            MailboxSender::User,
        ],
    );
    let canonical = snapshot.context().unwrap().item;
    let mut input = vec![canonical.clone()];
    let refs = HashMap::from([(receiver, 1), (advertised, 7)]);
    project_mailbox_inventories(&mut input, receiver, &refs);
    let rows = snapshot
        .pending_senders
        .iter()
        .map(|group| {
            let from = match group.sender {
                MailboxSender::Agent(id) if id == advertised => "7".to_string(),
                MailboxSender::Agent(id) => id.to_string(),
                MailboxSender::User => "user".to_string(),
            };
            json!({"from": from, "count": group.count})
        })
        .collect::<Vec<_>>();
    let expected_json = json!({"receiver": "1", "pending_senders": rows});
    let mut expected = canonical.clone();
    if let ResponseItem::Message { content, .. } = &mut expected {
        *content = vec![ContentItem::InputText {
            text: format!("{HEADING}{expected_json}{GUIDANCE}"),
        }];
    }
    assert_eq!(input, vec![expected]);
    assert_eq!(snapshot.context().unwrap().item, canonical);
    // A fresh namespace must reproject from canonical UUIDs, not an old ref.
    let mut refreshed = vec![canonical];
    project_mailbox_inventories(&mut refreshed, receiver, &HashMap::from([(advertised, 9)]));
    let projected: Value = serde_json::from_str(
        text(&refreshed[0])
            .strip_prefix(HEADING)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(projected["receiver"], receiver.to_string());
    assert!(
        projected["pending_senders"]
            .as_array()
            .unwrap()
            .contains(&json!({"from": "9", "count": 1}))
    );
    let once = refreshed.clone();
    project_mailbox_inventories(&mut refreshed, receiver, &refs);
    assert_eq!(
        refreshed, once,
        "already projected text is not canonical input"
    );
}

#[derive(Clone, Copy)]
enum InvalidArtifact {
    UserRole,
    ProviderId,
    DifferentTurn,
    DifferentReceiver,
    ExtraContent,
    ChangedGuidance,
    UnknownField,
    NonCanonicalSender,
    InvalidFrontier,
}

#[test_case(InvalidArtifact::UserRole; "user cannot impersonate inventory")]
#[test_case(InvalidArtifact::ProviderId; "ordinary provider message")]
#[test_case(InvalidArtifact::DifferentTurn; "notification turn mismatch")]
#[test_case(InvalidArtifact::DifferentReceiver; "foreign inventory")]
#[test_case(InvalidArtifact::ExtraContent; "mixed content")]
#[test_case(InvalidArtifact::ChangedGuidance; "marker-looking body")]
#[test_case(InvalidArtifact::UnknownField; "unknown schema")]
#[test_case(InvalidArtifact::NonCanonicalSender; "noncanonical sender key")]
#[test_case(InvalidArtifact::InvalidFrontier; "invalid fixed frontier")]
#[test]
fn leaves_noncanonical_or_untrusted_messages_unchanged(case: InvalidArtifact) {
    let receiver = ThreadId::new();
    let snapshot = notification(receiver, vec![MailboxSender::Agent(ThreadId::new())]);
    let mut item = snapshot.context().unwrap().item;
    let mut target = receiver;
    match case {
        InvalidArtifact::ProviderId => {
            item.set_id(Some(codex_protocol::ResponseItemId::new("msg")))
        }
        InvalidArtifact::DifferentReceiver => target = ThreadId::new(),
        InvalidArtifact::DifferentTurn => {
            if let ResponseItem::Message {
                internal_chat_message_metadata_passthrough,
                ..
            } = &mut item
            {
                *internal_chat_message_metadata_passthrough = None;
            }
            item.set_turn_id_if_missing("other-turn");
        }
        InvalidArtifact::UserRole
        | InvalidArtifact::ExtraContent
        | InvalidArtifact::ChangedGuidance
        | InvalidArtifact::UnknownField
        | InvalidArtifact::NonCanonicalSender
        | InvalidArtifact::InvalidFrontier => {
            let ResponseItem::Message { role, content, .. } = &mut item else {
                panic!("message");
            };
            match case {
                InvalidArtifact::UserRole => *role = "user".to_string(),
                InvalidArtifact::ExtraContent => content.push(ContentItem::InputText {
                    text: "extra".to_string(),
                }),
                InvalidArtifact::ChangedGuidance => {
                    let ContentItem::InputText { text } = &mut content[0] else {
                        panic!("text");
                    };
                    text.push_str(" Changed.");
                }
                InvalidArtifact::UnknownField
                | InvalidArtifact::NonCanonicalSender
                | InvalidArtifact::InvalidFrontier => {
                    let ContentItem::InputText { text } = &mut content[0] else {
                        panic!("text");
                    };
                    let (body, suffix) = text
                        .strip_prefix(HEADING)
                        .unwrap()
                        .split_once('\n')
                        .unwrap();
                    let mut body: Value = serde_json::from_str(body).unwrap();
                    match case {
                        InvalidArtifact::UnknownField => body["unknown"] = json!(true),
                        InvalidArtifact::NonCanonicalSender => {
                            body["pending_senders"][0]["sender_key"] = json!("agent:2");
                        }
                        InvalidArtifact::InvalidFrontier => body["through_sequence"] = json!(0),
                        InvalidArtifact::UserRole
                        | InvalidArtifact::ProviderId
                        | InvalidArtifact::DifferentTurn
                        | InvalidArtifact::DifferentReceiver
                        | InvalidArtifact::ExtraContent
                        | InvalidArtifact::ChangedGuidance => unreachable!(),
                    }
                    *text = format!("{HEADING}{body}\n{suffix}");
                }
                InvalidArtifact::ProviderId
                | InvalidArtifact::DifferentTurn
                | InvalidArtifact::DifferentReceiver => unreachable!(),
            }
        }
    }
    let expected = vec![item];
    let mut input = expected.clone();
    project_mailbox_inventories(&mut input, target, &HashMap::from([(receiver, 1)]));
    assert_eq!(input, expected);
}

#[test]
fn bounds_many_senders_with_explicit_prose_omission_and_no_extra_json_metadata() {
    let receiver = ThreadId::new();
    let snapshot = notification(
        receiver,
        (0..100)
            .map(|_| MailboxSender::Agent(ThreadId::new()))
            .collect(),
    );
    let canonical = snapshot.context().unwrap().item;
    let mut input = vec![canonical.clone()];
    project_mailbox_inventories(&mut input, receiver, &HashMap::new());
    let rendered = text(&input[0]);
    assert!(rendered.len() <= approx_bytes_for_tokens(INVENTORY_TOKENS));
    assert!(rendered.ends_with(OMITTED));
    let body: Value = serde_json::from_str(
        rendered
            .strip_prefix(HEADING)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let rows = body["pending_senders"].as_array().unwrap();
    assert!(!rows.is_empty());
    assert!(rows.len() < snapshot.pending_senders.len());
    let expected_rows = snapshot.pending_senders[..rows.len()]
        .iter()
        .map(|group| {
            let MailboxSender::Agent(id) = group.sender else {
                panic!("agent");
            };
            json!({"from": id.to_string(), "count": group.count})
        })
        .collect::<Vec<_>>();
    assert_eq!(
        body,
        json!({"receiver": receiver.to_string(), "pending_senders": expected_rows})
    );
    assert_eq!(snapshot.context().unwrap().item, canonical);
}
