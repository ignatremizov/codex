use super::*;
use crate::StateRuntime;
use crate::migrations::QUEUE_MIGRATOR;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrator;
use std::borrow::Cow;
use std::sync::Arc;

async fn runtime() -> Arc<StateRuntime> {
    let home = unique_temp_dir();
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap()
}

fn invocation(receiver_thread_id: ThreadId, tool_call_id: &str) -> MailboxInvocation {
    MailboxInvocation {
        receiver_thread_id,
        turn_id: "receiver-turn".to_string(),
        tool_call_id: tool_call_id.to_string(),
    }
}

fn claimed(mut message: MailboxMessage) -> MailboxMessage {
    message.state = MailboxMessageState::Claimed;
    message
}

fn assert_invalid_input(error: anyhow::Error) {
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn recovery_lists_only_outstanding_claims_with_full_membership_in_acceptance_order() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let empty_invocation = invocation(receiver, "empty");
    let empty = queue
        .claim_mail(&empty_invocation, &MailboxSelection::All)
        .await
        .unwrap();
    for (key, sender) in [
        ("first", "a"),
        ("second", "b"),
        ("third", "a"),
        ("fourth", "a"),
    ] {
        queue
            .accept_mail(receiver, key, sender, "{}")
            .await
            .unwrap();
    }
    // Claim creation order differs from the earliest member's acceptance order.
    let second = queue
        .claim_mail(
            &invocation(receiver, "second"),
            &MailboxSelection::Senders(vec!["b".to_string()]),
        )
        .await
        .unwrap();
    let mut first = queue
        .claim_mail(
            &invocation(receiver, "first"),
            &MailboxSelection::Senders(vec!["a".to_string()]),
        )
        .await
        .unwrap();
    first.messages[0].message = queue
        .acknowledge_mail(
            &first.invocation,
            &first.messages[0].message.id,
            &first.messages[0].delivery_id,
        )
        .await
        .unwrap();
    first.messages[1].message = queue
        .reject_mail(receiver, &first.messages[1].message.id, "revoked")
        .await
        .unwrap();
    let pending = queue
        .accept_mail(receiver, "unclaimed", "user", "{}")
        .await
        .unwrap();
    let foreign = ThreadId::new();
    queue
        .accept_mail(foreign, "foreign", "user", "{}")
        .await
        .unwrap();
    let foreign_claim = queue
        .claim_mail(&invocation(foreign, "foreign"), &MailboxSelection::All)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            queue.list_unconsumed_mail_claims(receiver).await.unwrap(),
            vec![first.clone(), second.clone()]
        );
    }
    assert_eq!(
        queue.read_mail(receiver, &pending.id).await.unwrap(),
        Some(pending)
    );
    assert_eq!(
        queue
            .claim_mail(&empty_invocation, &MailboxSelection::All)
            .await
            .unwrap(),
        empty
    );
    queue
        .acknowledge_mail(
            &first.invocation,
            &first.messages[2].message.id,
            &first.messages[2].delivery_id,
        )
        .await
        .unwrap();
    assert_eq!(
        queue.list_unconsumed_mail_claims(receiver).await.unwrap(),
        vec![second.clone()]
    );
    queue
        .reject_mail(receiver, &second.messages[0].message.id, "revoked")
        .await
        .unwrap();
    assert_eq!(
        queue.list_unconsumed_mail_claims(receiver).await.unwrap(),
        Vec::<MailboxClaim>::new()
    );
    assert_eq!(
        queue.list_unconsumed_mail_claims(foreign).await.unwrap(),
        vec![foreign_claim]
    );
}

#[tokio::test]
async fn acceptance_is_immutable_idempotent_and_receiver_scoped() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let first = queue
        .accept_mail(receiver, "submission", "user", r#"{"text":"hello"}"#)
        .await
        .unwrap();
    assert_eq!(
        first,
        queue
            .accept_mail(receiver, "submission", "user", r#"{"text":"hello"}"#)
            .await
            .unwrap()
    );
    for (sender, payload) in [
        ("user".to_string(), r#"{"text":"different"}"#),
        (ThreadId::new().to_string(), r#"{"text":"hello"}"#),
    ] {
        assert_invalid_input(
            queue
                .accept_mail(receiver, "submission", &sender, payload)
                .await
                .unwrap_err(),
        );
    }
    assert_eq!(
        Some(first.clone()),
        queue.read_mail(receiver, &first.id).await.unwrap()
    );
    let other_receiver = ThreadId::new();
    let other = queue
        .accept_mail(other_receiver, "submission", "user", r#"{"other":true}"#)
        .await
        .unwrap();
    assert_ne!(first.id, other.id);
    assert_eq!(
        None,
        queue.read_mail(other_receiver, &first.id).await.unwrap()
    );
}

#[tokio::test]
async fn empty_claims_remain_empty_after_arrivals_and_reopen() {
    let runtime = runtime().await;
    let receiver = ThreadId::new();
    let call = invocation(receiver, "empty-check");
    let empty = runtime
        .thread_queue()
        .claim_mail(&call, &MailboxSelection::All)
        .await
        .unwrap();
    assert_eq!(
        empty,
        MailboxClaim {
            invocation: call.clone(),
            selection: MailboxSelection::All,
            messages: Vec::new(),
        }
    );
    let arrival = runtime
        .thread_queue()
        .accept_mail(receiver, "late", "user", r#"{"late":true}"#)
        .await
        .unwrap();
    let sqlite = runtime.sqlite().clone();
    runtime.close().await;
    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .unwrap();
    let queue = reopened.thread_queue();
    assert_eq!(
        empty,
        queue
            .claim_mail(&call, &MailboxSelection::All)
            .await
            .unwrap()
    );
    assert_invalid_input(
        queue
            .claim_mail(&call, &MailboxSelection::Senders(vec!["user".to_string()]))
            .await
            .unwrap_err(),
    );
    assert!(
        queue
            .claim_mail(
                &invocation(receiver, "no-senders"),
                &MailboxSelection::Senders(Vec::new()),
            )
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    let next = queue
        .claim_mail(&invocation(receiver, "next-check"), &MailboxSelection::All)
        .await
        .unwrap();
    assert_eq!(
        vec![claimed(arrival)],
        next.messages
            .into_iter()
            .map(|member| member.message)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn sender_union_preserves_order_and_retry_membership_without_draining_other_mail() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let agent = ThreadId::new().to_string();
    let unrelated = ThreadId::new().to_string();
    let mut selected = Vec::new();
    let mut unselected = Vec::new();
    for (key, sender) in [
        ("one", "user"),
        ("two", unrelated.as_str()),
        ("three", agent.as_str()),
        ("four", "user"),
    ] {
        let message = queue
            .accept_mail(receiver, key, sender, "{}")
            .await
            .unwrap();
        if sender == unrelated {
            unselected.push(message);
        } else {
            selected.push(claimed(message));
        }
    }
    let ordinary = queue
        .enqueue(receiver, r#"{"ordinary":true}"#)
        .await
        .unwrap();
    let foreign = queue
        .accept_mail(ThreadId::new(), "foreign", "user", "{}")
        .await
        .unwrap();
    let call = invocation(receiver, "selected");
    let selection =
        MailboxSelection::Senders(vec!["user".to_string(), agent.clone(), agent.clone()]);
    let first = queue.claim_mail(&call, &selection).await.unwrap();
    assert_eq!(
        selected,
        first
            .messages
            .iter()
            .map(|member| member.message.clone())
            .collect::<Vec<_>>()
    );
    let later = queue
        .accept_mail(receiver, "later", &agent, r#"{"later":true}"#)
        .await
        .unwrap();
    unselected.push(later);
    assert_eq!(
        first,
        queue
            .claim_mail(
                &call,
                &MailboxSelection::Senders(vec![agent, "user".to_string()])
            )
            .await
            .unwrap()
    );
    assert_invalid_input(
        queue
            .claim_mail(&call, &MailboxSelection::All)
            .await
            .unwrap_err(),
    );
    let rest = queue
        .claim_mail(&invocation(receiver, "rest"), &MailboxSelection::All)
        .await
        .unwrap();
    assert_eq!(
        unselected.into_iter().map(claimed).collect::<Vec<_>>(),
        rest.messages
            .into_iter()
            .map(|member| member.message)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        vec![ordinary],
        queue
            .list_page(receiver, /*offset*/ 0, /*limit*/ 1)
            .await
            .unwrap()
    );
    assert_eq!(
        Some(foreign.clone()),
        queue
            .read_mail(foreign.receiver_thread_id, &foreign.id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn competing_runtimes_accept_and_claim_the_same_invocation_idempotently() {
    let runtime = runtime().await;
    let other = StateRuntime::init(runtime.sqlite().clone(), "test-provider".to_string())
        .await
        .unwrap();
    let receiver = ThreadId::new();
    let (first, second) = tokio::join!(
        runtime
            .thread_queue()
            .accept_mail(receiver, "same", "user", "{}"),
        other
            .thread_queue()
            .accept_mail(receiver, "same", "user", "{}"),
    );
    let accepted = first.unwrap();
    assert_eq!(accepted, second.unwrap());
    let call = invocation(receiver, "same-call");
    let (first, second) = tokio::join!(
        runtime
            .thread_queue()
            .claim_mail(&call, &MailboxSelection::All),
        other
            .thread_queue()
            .claim_mail(&call, &MailboxSelection::All),
    );
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    assert_eq!(
        vec![claimed(accepted)],
        first
            .messages
            .into_iter()
            .map(|member| member.message)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn competing_claims_never_share_members_and_identity_includes_turn_and_receiver() {
    let runtime = runtime().await;
    let other = StateRuntime::init(runtime.sqlite().clone(), "test-provider".to_string())
        .await
        .unwrap();
    let receiver = ThreadId::new();
    let mut expected = Vec::new();
    for key in ["one", "two", "three"] {
        expected.push(claimed(
            runtime
                .thread_queue()
                .accept_mail(receiver, key, "user", "{}")
                .await
                .unwrap(),
        ));
    }
    let first_call = invocation(receiver, "same-tool-call");
    let mut second_call = first_call.clone();
    second_call.turn_id = "different-turn".to_string();
    let (first, second) = tokio::join!(
        runtime
            .thread_queue()
            .claim_mail(&first_call, &MailboxSelection::All),
        other
            .thread_queue()
            .claim_mail(&second_call, &MailboxSelection::All),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert!(first.messages.is_empty() || second.messages.is_empty());
    let mut actual = first
        .messages
        .into_iter()
        .chain(second.messages)
        .map(|member| member.message)
        .collect::<Vec<_>>();
    actual.sort_by_key(|message| message.acceptance_sequence);
    assert_eq!(expected, actual);

    let another_receiver = ThreadId::new();
    let accepted = runtime
        .thread_queue()
        .accept_mail(another_receiver, "one", "user", "{}")
        .await
        .unwrap();
    let other_claim = runtime
        .thread_queue()
        .claim_mail(
            &invocation(another_receiver, "same-tool-call"),
            &MailboxSelection::All,
        )
        .await
        .unwrap();
    assert_eq!(
        vec![claimed(accepted)],
        other_claim
            .messages
            .into_iter()
            .map(|member| member.message)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn terminal_outcomes_preserve_payloads_and_claims_across_reopen() {
    let runtime = runtime().await;
    let queue = runtime.thread_queue();
    let receiver = ThreadId::new();
    let consumed = queue
        .accept_mail(receiver, "consumed", "user", r#"{"full":"payload"}"#)
        .await
        .unwrap();
    let rejected = queue
        .accept_mail(receiver, "rejected", "user", "{}")
        .await
        .unwrap();
    let still_claimed = queue
        .accept_mail(receiver, "still-claimed", "user", "{}")
        .await
        .unwrap();
    let pending_rejection = queue
        .accept_mail(receiver, "pending-rejection", "user", "{}")
        .await
        .unwrap();
    let mut expected_pending_rejection = pending_rejection.clone();
    expected_pending_rejection.state = MailboxMessageState::Rejected;
    expected_pending_rejection.rejection_reason = Some("revoked".to_string());
    assert_eq!(
        expected_pending_rejection,
        queue
            .reject_mail(receiver, &pending_rejection.id, "revoked")
            .await
            .unwrap()
    );
    let call = invocation(receiver, "consume");
    let mut expected = queue
        .claim_mail(&call, &MailboxSelection::All)
        .await
        .unwrap();
    assert_eq!(
        vec![
            claimed(consumed.clone()),
            claimed(rejected.clone()),
            claimed(still_claimed)
        ],
        expected
            .messages
            .iter()
            .map(|member| member.message.clone())
            .collect::<Vec<_>>()
    );
    let delivery_id = expected.messages[0].delivery_id.clone();
    for wrong_call in [
        invocation(receiver, "wrong-call"),
        invocation(ThreadId::new(), "consume"),
        MailboxInvocation {
            turn_id: "wrong-turn".to_string(),
            ..call.clone()
        },
    ] {
        assert_invalid_input(
            queue
                .acknowledge_mail(&wrong_call, &consumed.id, &delivery_id)
                .await
                .unwrap_err(),
        );
    }
    assert_invalid_input(
        queue
            .acknowledge_mail(&call, &consumed.id, "wrong-delivery")
            .await
            .unwrap_err(),
    );
    assert_invalid_input(
        queue
            .reject_mail(ThreadId::new(), &consumed.id, "revoked")
            .await
            .unwrap_err(),
    );

    expected.messages[0].message.state = MailboxMessageState::Consumed;
    expected.messages[1].message.state = MailboxMessageState::Rejected;
    expected.messages[1].message.rejection_reason = Some("revoked".to_string());
    assert_eq!(
        expected.messages[0].message,
        queue
            .acknowledge_mail(&call, &consumed.id, &delivery_id)
            .await
            .unwrap()
    );
    assert_eq!(
        expected.messages[1].message,
        queue
            .reject_mail(receiver, &rejected.id, "revoked")
            .await
            .unwrap()
    );
    let sqlite = runtime.sqlite().clone();
    runtime.close().await;
    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .unwrap();
    let queue = reopened.thread_queue();
    assert_eq!(
        expected,
        queue
            .claim_mail(&call, &MailboxSelection::All)
            .await
            .unwrap()
    );
    assert_eq!(
        expected.messages[0].message,
        queue
            .acknowledge_mail(&call, &consumed.id, &delivery_id)
            .await
            .unwrap()
    );
    assert_eq!(
        expected.messages[1].message,
        queue
            .reject_mail(receiver, &rejected.id, "revoked")
            .await
            .unwrap()
    );
    assert_eq!(
        expected.messages[0].message,
        queue
            .accept_mail(receiver, "consumed", "user", r#"{"full":"payload"}"#)
            .await
            .unwrap()
    );
    assert_invalid_input(
        queue
            .reject_mail(receiver, &consumed.id, "revoked")
            .await
            .unwrap_err(),
    );
    assert_invalid_input(
        queue
            .reject_mail(receiver, &rejected.id, "different")
            .await
            .unwrap_err(),
    );
    assert_invalid_input(
        queue
            .acknowledge_mail(&call, &rejected.id, &expected.messages[1].delivery_id)
            .await
            .unwrap_err(),
    );
    assert_eq!(
        Some(expected_pending_rejection),
        queue
            .read_mail(receiver, &pending_rejection.id)
            .await
            .unwrap()
    );
    assert!(
        queue
            .claim_mail(&invocation(receiver, "later"), &MailboxSelection::All)
            .await
            .unwrap()
            .messages
            .is_empty()
    );
}

#[tokio::test]
async fn competing_terminal_transitions_cannot_overwrite_each_other() {
    let runtime = runtime().await;
    let other = StateRuntime::init(runtime.sqlite().clone(), "test-provider".to_string())
        .await
        .unwrap();
    let receiver = ThreadId::new();
    let accepted = runtime
        .thread_queue()
        .accept_mail(receiver, "one", "user", "{}")
        .await
        .unwrap();
    let call = invocation(receiver, "consume");
    let claim = runtime
        .thread_queue()
        .claim_mail(&call, &MailboxSelection::All)
        .await
        .unwrap();
    let delivery_id = &claim.messages[0].delivery_id;
    let (acknowledged, rejected) = tokio::join!(
        runtime
            .thread_queue()
            .acknowledge_mail(&call, &accepted.id, delivery_id),
        other
            .thread_queue()
            .reject_mail(receiver, &accepted.id, "revoked"),
    );
    let outcome = match (acknowledged, rejected) {
        (Ok(message), Err(error)) | (Err(error), Ok(message)) => {
            assert_invalid_input(error);
            message
        }
        outcomes => panic!("expected exactly one terminal transition to succeed: {outcomes:?}"),
    };
    assert_eq!(
        Some(outcome),
        runtime
            .thread_queue()
            .read_mail(receiver, &accepted.id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn mailbox_migration_preserves_existing_queue_payload_order_and_revisions() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home).await.unwrap();
    let sqlite = crate::SqliteConfig::new_for_testing(home.as_path().abs());
    let old_migrator = Migrator {
        migrations: Cow::Owned(
            QUEUE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= 2)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
        table_name: QUEUE_MIGRATOR.table_name.clone(),
        create_schemas: QUEUE_MIGRATOR.create_schemas.clone(),
    };
    let pool = Arc::new(
        sqlite
            .open_read_write_pool(&sqlite.queue_db_path())
            .await
            .unwrap(),
    );
    old_migrator.run(pool.as_ref()).await.unwrap();
    let queue = SqliteQueueStore::new(Arc::clone(&pool));
    let receiver = ThreadId::new();
    let first = queue.enqueue(receiver, r#"{"first":true}"#).await.unwrap();
    let second = queue.enqueue(receiver, r#"{"second":true}"#).await.unwrap();
    queue
        .reorder(receiver, &[second.id.clone(), first.id.clone()])
        .await
        .unwrap();
    let revisions = queue
        .changes_since(/*revision*/ 0, &[receiver])
        .await
        .unwrap();
    QUEUE_MIGRATOR.run(pool.as_ref()).await.unwrap();
    let accepted = queue
        .accept_mail(receiver, "new-mail", "user", "{}")
        .await
        .unwrap();
    let call = invocation(receiver, "new-claim");
    let claim = queue
        .claim_mail(&call, &MailboxSelection::All)
        .await
        .unwrap();
    queue
        .acknowledge_mail(&call, &accepted.id, &claim.messages[0].delivery_id)
        .await
        .unwrap();
    assert_eq!(
        vec![second, first],
        queue
            .list_page(receiver, /*offset*/ 0, /*limit*/ 2)
            .await
            .unwrap()
    );
    assert_eq!(
        revisions,
        queue
            .changes_since(/*revision*/ 0, &[receiver])
            .await
            .unwrap()
    );
}
