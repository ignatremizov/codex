use super::*;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::ClaimMailboxInputParams;
use codex_thread_store::ClaimedMailboxInput;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::MailboxPayload;
use codex_thread_store::StoredMailboxInput;
use codex_thread_store::ThreadStoreFuture;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

fn claim(receiver: ThreadId, call_id: &str, states: Vec<MailboxMessageState>) -> MailboxClaim {
    MailboxClaim {
        invocation: MailboxInvocation {
            receiver_thread_id: receiver,
            turn_id: "turn".to_string(),
            tool_call_id: call_id.to_string(),
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
                    final_subscription: None,
                },
            })
            .collect(),
    }
}

struct FixedClaimStore {
    inner: InMemoryThreadStore,
    fixed_claim: Option<MailboxClaim>,
    claim_lookups: AtomicUsize,
}

macro_rules! forward_store {
    ($(fn $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)*) => {
        $(fn $method(&self, $($arg: $ty),*) -> ThreadStoreFuture<'_, $result> {
            self.inner.$method($($arg),*)
        })*
    };
}

impl ThreadStore for FixedClaimStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    forward_store! {
        fn accept_mailbox_input(params: codex_thread_store::AcceptMailboxInputParams) -> codex_thread_store::StoredMailboxInput;
        fn claim_mailbox_input(params: codex_thread_store::ClaimMailboxInputParams) -> codex_thread_store::MailboxClaim;
        fn create_thread(params: codex_thread_store::CreateThreadParams) -> ();
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> ();
        fn reserve_thread_writers(thread_ids: Vec<ThreadId>) -> codex_thread_store::ThreadWriterReservation;
        fn append_items(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn append_completion_items_and_flush(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn persist_thread(thread_id: ThreadId, context: codex_thread_store::PersistContext) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn load_sub_agent_completion_context_item(params: codex_thread_store::LoadSubAgentCompletionContextItemParams) -> Option<codex_protocol::models::ResponseItem>;
        fn load_sub_agent_completion_presentation(params: codex_thread_store::LoadSubAgentCompletionPresentationParams) -> codex_thread_store::StoredSubAgentCompletionPresentation;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
    }

    fn lookup_mailbox_claim(
        &self,
        invocation: MailboxInvocation,
    ) -> ThreadStoreFuture<'_, Option<MailboxClaim>> {
        self.claim_lookups.fetch_add(1, Ordering::SeqCst);
        if let Some(claim) = self
            .fixed_claim
            .as_ref()
            .filter(|claim| claim.invocation == invocation)
        {
            let claim = claim.clone();
            Box::pin(async move { Ok(Some(claim)) })
        } else {
            self.inner.lookup_mailbox_claim(invocation)
        }
    }
}

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
fn terminal_claim_outcomes_describe_the_fixed_batch_and_nonterminal_is_not_delivery() {
    let receiver = ThreadId::new();
    for (states, expected) in [
        (vec![], ClaimOutcome::Empty),
        (
            vec![MailboxMessageState::Consumed, MailboxMessageState::Consumed],
            ClaimOutcome::Success { rejected_count: 0 },
        ),
        (
            vec![MailboxMessageState::Consumed, MailboxMessageState::Rejected],
            ClaimOutcome::Success { rejected_count: 1 },
        ),
        (
            vec![MailboxMessageState::Rejected],
            ClaimOutcome::Rejected { rejected_count: 1 },
        ),
        (
            vec![MailboxMessageState::Rejected, MailboxMessageState::Rejected],
            ClaimOutcome::Rejected { rejected_count: 2 },
        ),
        (
            vec![MailboxMessageState::Consumed, MailboxMessageState::Claimed],
            ClaimOutcome::Nonterminal,
        ),
        (
            vec![MailboxMessageState::Pending],
            ClaimOutcome::Nonterminal,
        ),
    ] {
        assert_eq!(
            ClaimSummary::from_claim(&claim(receiver, "check", states)).unwrap(),
            ClaimSummary {
                from: None,
                outcome: expected,
            }
        );
    }
}

#[tokio::test]
async fn successful_projection_keeps_the_tool_result_pair_without_a_redundant_status() {
    let receiver = ThreadId::new();
    for (call_id, states, expected) in [
        ("success", vec![MailboxMessageState::Consumed], None),
        (
            "mixed",
            vec![MailboxMessageState::Consumed, MailboxMessageState::Rejected],
            Some(json!({"rejected_count": 1})),
        ),
        ("empty", vec![], Some(json!({"status": "empty"}))),
        (
            "rejected",
            vec![MailboxMessageState::Rejected],
            Some(json!({"status": "rejected", "rejected_count": 1})),
        ),
    ] {
        let original = pair(call_id, json!({"status":"delivery_requested","from":null}));
        let mut input = original.clone();
        let store = FixedClaimStore {
            inner: InMemoryThreadStore::default(),
            fixed_claim: Some(claim(receiver, call_id, states)),
            claim_lookups: AtomicUsize::default(),
        };
        project_check_mail_results(&mut input, receiver, &store, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(store.claim_lookups.load(Ordering::SeqCst), 1);

        let mut expected_items = original;
        let ResponseItem::FunctionCallOutput { output, .. } = &mut expected_items[1] else {
            panic!("expected paired check_mail result");
        };
        output.body = FunctionCallOutputBody::Text(
            expected.map_or_else(String::new, |expected| expected.to_string()),
        );
        assert_eq!(input, expected_items);
    }
}

#[tokio::test]
async fn projection_requires_canonical_acceptance_and_never_creates_or_expands_a_claim() {
    let store = FixedClaimStore {
        inner: InMemoryThreadStore::default(),
        fixed_claim: None,
        claim_lookups: AtomicUsize::default(),
    };
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
            final_subscription: Default::default(),
        })
        .await
        .unwrap();
    let mut input = raw.clone();
    input.push(raw[1].clone());
    let lookups_before_projection = store.claim_lookups.load(Ordering::SeqCst);
    project_check_mail_results(&mut input, receiver, &store, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(
        store.claim_lookups.load(Ordering::SeqCst) - lookups_before_projection,
        1,
        "duplicate tool results share one fixed-claim lookup"
    );
    let mut expected = pair(
        "check",
        json!({
            "status":"empty",
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
        json!({"status":"ok"}),
        json!({"status":"ok","rejected_count":1}),
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
async fn terminal_receipts_omit_sender_but_still_require_canonical_acceptance() {
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
    for refs in [
        HashMap::from([(sender, 2)]),
        HashMap::from([(sender, 7)]),
        HashMap::new(),
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
                    "status":"empty",
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
async fn nonterminal_acceptance_keeps_current_receiver_refs_and_never_reports_ok() {
    let store = InMemoryThreadStore::default();
    let receiver = ThreadId::new();
    let sender = ThreadId::new();
    let identity = |thread_id| codex_protocol::AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "pending-agent".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![codex_protocol::user_input::UserInput::Text {
                    text: "Unconsumed mail.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(codex_protocol::AgentInputAttribution {
                    sender: identity(sender),
                    recipient: identity(receiver),
                    sender_turn_id: "sender-turn".to_string(),
                    batch_id: None,
                }),
            },
            final_subscription: Default::default(),
        })
        .await
        .unwrap();
    let claim = store
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
    assert_eq!(claim.messages.len(), 1);
    let canonical = pair(
        "check",
        json!({"status": "delivery_requested", "from": sender.to_string()}),
    );
    for (refs, from) in [
        (HashMap::from([(sender, 2)]), "2".to_string()),
        (HashMap::from([(sender, 7)]), "7".to_string()),
        (HashMap::new(), sender.to_string()),
    ] {
        let mut input = canonical.clone();
        project_check_mail_results(&mut input, receiver, &store, &refs)
            .await
            .unwrap();
        assert_eq!(
            structured(&input),
            structured(&pair(
                "check",
                json!({"status": "delivery_requested", "from": from})
            )),
        );
    }
    assert_eq!(
        store
            .lookup_mailbox_claim(claim.invocation.clone())
            .await
            .unwrap(),
        Some(claim),
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
