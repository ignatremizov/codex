use super::*;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::ClaimMailboxInputParams;
use codex_thread_store::ClaimedMailboxInput;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::MailboxPayload;
use codex_thread_store::StoredMailboxInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn pair(call_id: &str, acceptance: Value) -> Vec<ResponseItem> {
    let mut items: Vec<ResponseItem> = serde_json::from_value(json!([
        {"type":"function_call", "name":"check_mail", "namespace":"multi_agent_v1",
         "call_id":call_id, "arguments":"{}"},
        {"type":"function_call_output", "call_id":call_id, "output":acceptance.to_string()},
    ]))
    .unwrap();
    for item in &mut items {
        item.set_turn_id_if_missing("turn");
    }
    items
}

fn structured(items: &[ResponseItem]) -> Value {
    let mut value = serde_json::to_value(items).unwrap();
    for item in value.as_array_mut().unwrap() {
        if item["type"] == "function_call_output" {
            item["output"] = serde_json::from_str(item["output"].as_str().unwrap()).unwrap();
        }
    }
    value
}

#[test]
fn terminal_counts_describe_the_fixed_batch_and_nonterminal_is_not_delivery() {
    let receiver = ThreadId::new();
    for (states, expected) in [
        (
            vec![],
            Some(json!({"status":"empty", "delivered_count":0, "rejected_count":0})),
        ),
        (
            vec![MailboxMessageState::Consumed, MailboxMessageState::Rejected],
            Some(json!({"status":"delivered", "delivered_count":1, "rejected_count":1})),
        ),
        (
            vec![MailboxMessageState::Rejected],
            Some(json!({"status":"rejected", "delivered_count":0, "rejected_count":1})),
        ),
        (
            vec![MailboxMessageState::Consumed, MailboxMessageState::Claimed],
            None,
        ),
        (vec![MailboxMessageState::Pending], None),
    ] {
        let claim = MailboxClaim {
            invocation: MailboxInvocation {
                receiver_thread_id: receiver,
                turn_id: "turn".to_string(),
                tool_call_id: "check".to_string(),
            },
            selection: MailboxSelection::All,
            messages: states
                .into_iter()
                .enumerate()
                .map(|(index, state)| ClaimedMailboxInput {
                    delivery_id: format!("delivery-{index}"),
                    message: StoredMailboxInput {
                        id: format!("message-{index}"),
                        receiver_thread_id: receiver,
                        submission_key: format!("submission-{index}"),
                        sender: MailboxSender::User,
                        payload: MailboxPayload::User {
                            input: Vec::new(),
                            client_id: None,
                        },
                        acceptance_sequence: index as i64,
                        state,
                        rejection_reason: None,
                    },
                })
                .collect(),
        };
        let summary = ClaimSummary::from_claim(&claim).unwrap();
        assert_eq!(summary.from, None);
        assert_eq!(
            summary
                .counts
                .map(|counts| serde_json::to_value(counts).unwrap()),
            expected
        );
    }
}

#[tokio::test]
async fn projection_requires_canonical_acceptance_and_never_creates_or_expands_a_claim() {
    let store = InMemoryThreadStore::default();
    let receiver = ThreadId::new();
    let invocation = MailboxInvocation {
        receiver_thread_id: receiver,
        turn_id: "turn".to_string(),
        tool_call_id: "check".to_string(),
    };
    let raw = pair("check", json!({"status":"delivery_requested","from":null}));
    let mut missing = raw.clone();
    project_check_mail_results(&mut missing, receiver, &store, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(missing, raw);
    assert_eq!(
        store
            .lookup_mailbox_claim(invocation.clone())
            .await
            .unwrap(),
        None
    );
    let claim = store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: invocation.clone(),
            selection: MailboxSelection::All,
        })
        .await
        .unwrap();
    store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "late".to_string(),
            payload: MailboxPayload::User {
                input: vec![codex_protocol::user_input::UserInput::Text {
                    text: "later arrival".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        })
        .await
        .unwrap();
    let mut input = raw.clone();
    input.push(raw[1].clone());
    project_check_mail_results(&mut input, receiver, &store, &HashMap::new())
        .await
        .unwrap();
    let mut expected = pair(
        "check",
        json!({
            "status":"empty", "from":null, "delivered_count":0, "rejected_count":0,
        }),
    );
    expected.push(expected[1].clone());
    assert_eq!(structured(&input), structured(&expected));
    assert_eq!(
        store.lookup_mailbox_claim(invocation).await.unwrap(),
        Some(claim)
    );
    assert_eq!(
        raw,
        pair("check", json!({"status":"delivery_requested","from":null}))
    );

    let inflight = MailboxInvocation {
        receiver_thread_id: receiver,
        turn_id: "turn".to_string(),
        tool_call_id: "inflight".to_string(),
    };
    let claimed = store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: inflight.clone(),
            selection: MailboxSelection::All,
        })
        .await
        .unwrap();
    assert_eq!(claimed.messages.len(), 1);
    let original = pair(
        "inflight",
        json!({"status":"delivery_requested","from":null}),
    );
    let mut input = original.clone();
    project_check_mail_results(&mut input, receiver, &store, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(structured(&input), structured(&original));
    assert_eq!(
        store.lookup_mailbox_claim(inflight).await.unwrap(),
        Some(claimed)
    );

    for custom in [
        json!({"status":"delivery_requested"}),
        json!({"status":"delivery_requested","from":null,"hook":"changed"}),
        json!({"status":"custom","from":null}),
        json!({"status":"delivery_requested","from":"user"}),
        json!({"status":"delivered","from":"2","delivered_count":9,"rejected_count":0}),
    ] {
        let original = pair("check", custom);
        let mut input = original.clone();
        project_check_mail_results(&mut input, receiver, &store, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(input, original);
    }
    let mut wrong_namespace = raw.clone();
    let ResponseItem::FunctionCall { namespace, .. } = &mut wrong_namespace[0] else {
        panic!("call");
    };
    *namespace = Some("other".to_string());
    let mut wait_result = raw.clone();
    let ResponseItem::FunctionCall { name, .. } = &mut wait_result[0] else {
        panic!("call");
    };
    *name = "wait_agent".to_string();
    let mut wrong_turn = raw.clone();
    let ResponseItem::FunctionCallOutput {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut wrong_turn[1]
    else {
        panic!("result");
    };
    *internal_chat_message_metadata_passthrough = None;
    wrong_turn[1].set_turn_id_if_missing("other-turn");
    let mut failed = raw.clone();
    let ResponseItem::FunctionCallOutput { output, .. } = &mut failed[1] else {
        panic!("result");
    };
    output.success = Some(false);
    for original in [
        wrong_namespace,
        wait_result,
        wrong_turn,
        failed,
        vec![raw[1].clone()],
    ] {
        let mut input = original.clone();
        project_check_mail_results(&mut input, receiver, &store, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(input, original);
    }
}

#[tokio::test]
async fn refs_come_from_current_receiver_mapping_not_a_prior_model_result() {
    let store = InMemoryThreadStore::default();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: MailboxInvocation {
                receiver_thread_id: receiver,
                turn_id: "turn".to_string(),
                tool_call_id: "check".to_string(),
            },
            selection: MailboxSelection::Senders(vec![MailboxSender::Agent(sender)]),
        })
        .await
        .unwrap();
    let raw = pair(
        "check",
        json!({"status":"delivery_requested","from":sender.to_string()}),
    );
    for (refs, from) in [
        (HashMap::from([(sender, 2)]), "2".to_string()),
        (HashMap::from([(sender, 7)]), "7".to_string()),
        (HashMap::new(), sender.to_string()),
    ] {
        let mut input = raw.clone();
        project_check_mail_results(&mut input, receiver, &store, &refs)
            .await
            .unwrap();
        assert_eq!(
            structured(&input),
            structured(&pair(
                "check",
                json!({
                    "status":"empty", "from":from, "delivered_count":0, "rejected_count":0,
                })
            ))
        );
    }
    let original = pair("check", json!({"status":"delivery_requested","from":"2"}));
    let mut input = original.clone();
    project_check_mail_results(&mut input, receiver, &store, &HashMap::from([(sender, 7)]))
        .await
        .unwrap();
    assert_eq!(
        input, original,
        "ref-only input cannot establish canonical sender identity"
    );
}

#[tokio::test]
async fn claim_lookup_failure_is_not_an_empty_mailbox() {
    use codex_utils_absolute_path::test_support::PathExt;

    let home = tempfile::tempdir().unwrap();
    let store = codex_thread_store::LocalThreadStore::new(
        codex_thread_store::LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(),
            sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
            default_model_provider_id: "test".to_string(),
        },
        /*state_db*/ None,
    );
    let original = pair("check", json!({"status":"delivery_requested","from":null}));
    let mut input = original.clone();
    let error = project_check_mail_results(&mut input, ThreadId::new(), &store, &HashMap::new())
        .await
        .expect_err("unavailable claim store must fail");
    assert!(
        error
            .to_string()
            .contains("failed to read fixed check_mail claim")
    );
    assert_eq!(input, original);
}
