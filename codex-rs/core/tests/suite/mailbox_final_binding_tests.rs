//! An exact mailbox final remains deliverable when binding is delayed past another turn.

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::MailboxInventoryAdmission;
use codex_core::TurnInputRequest;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::MailboxFinalSubscriptionState;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::ThreadIdle;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::timeout;

#[path = "mailbox_final_binding_store_tests.rs"]
mod proof_store;
use proof_store::DelayedProofStore;
use proof_store::ProofGateState;

const TIMEOUT: Duration = Duration::from_secs(/*secs*/ 15);

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingSource {
    Consumption,
    Inventory,
}

async fn begin(thread: &CodexThread, prompt: &str) -> Result<String> {
    let submission = thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started {
        turn_id,
        root_turn_id: _,
    } = submission
    else {
        anyhow::bail!("expected new turn for {prompt}: {submission:?}");
    };
    Ok(turn_id)
}

async fn finish(thread: &CodexThread, turn_id: &str) -> Result<()> {
    let completed = timeout(
        TIMEOUT,
        wait_for_event_match(thread, |event| match event {
            EventMsg::TurnComplete(event) if event.turn_id == turn_id => Some(event.clone()),
            _ => None,
        }),
    )
    .await
    .with_context(|| format!("completion of {turn_id}"))?;
    assert!(completed.error.is_none(), "turn failed: {completed:?}");
    ThreadIdle::wait(thread).await;
    Ok(())
}

#[test_case(BindingSource::Consumption, ThreadHistoryMode::Legacy; "consumption_legacy")]
#[test_case(BindingSource::Consumption, ThreadHistoryMode::Paginated; "consumption_paginated")]
#[test_case(BindingSource::Inventory, ThreadHistoryMode::Legacy; "inventory_legacy")]
#[test_case(BindingSource::Inventory, ThreadHistoryMode::Paginated; "inventory_paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_sibling_mailbox_binding_delivers_the_original_final(
    binding_source: BindingSource,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PAYLOAD: &str = "mail that selects the original receiver turn";
    const BOUND_FINAL: &str = "the original bound answer";
    const LATER_FINAL: &str = "the later answer must not replace it";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let home = Arc::new(TempDir::new()?);
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let state = StateRuntime::init(sqlite.clone(), "openai".into()).await?;
    let inner = Arc::new(LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(),
            sqlite,
            default_model_provider_id: "openai".into(),
        },
        Some(state),
    ));
    let (release_proof, proof_gate) = watch::channel(ProofGateState::Blocked);
    let store = Arc::new(DelayedProofStore {
        inner,
        blocked_sender: Mutex::new(None),
        entered: Notify::new(),
        release: proof_gate,
    });
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_home(home)
        .with_thread_store(store.clone())
        .with_history_mode(history_mode)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("not V2");
        })
        .build_with_streaming_server_auto_env(&server)
        .await?;
    // Siblings have no earlier S->R observer/native-parent route that could rescue A.
    let sender_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let receiver_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let sender = test.thread_manager.get_thread(sender_id).await?;
    let receiver = test.thread_manager.get_thread(receiver_id).await?;
    assert_eq!(sender.config_snapshot().await.history_mode, history_mode);
    assert_eq!(receiver.config_snapshot().await.history_mode, history_mode);
    let (release_busy, busy_gate) = oneshot::channel();
    let mut busy = server
        .mount_response(
            |request| request.body_contains_text("hold receiver before conditional acceptance"),
            vec![StreamingSseChunk {
                gate: Some(busy_gate),
                body: if binding_source == BindingSource::Consumption {
                    sse(vec![
                        ev_response_created("late-binding-busy"),
                        ev_function_call_with_namespace(
                            "late-check-mail",
                            "multi_agent_v1",
                            "check_mail",
                            "{}",
                        ),
                        ev_completed("late-binding-busy"),
                    ])
                } else {
                    sse(vec![
                        ev_response_created("late-binding-unrelated-busy"),
                        ev_assistant_message("unrelated-busy", "finished before inventory"),
                        ev_completed("late-binding-unrelated-busy"),
                    ])
                },
            }],
        )
        .await;
    let busy_turn = begin(&receiver, "hold receiver before conditional acceptance").await?;
    timeout(TIMEOUT, busy.wait_for_request()).await?;
    let args =
        json!({"target": receiver_id.to_string(), "message": PAYLOAD, "w": "zf"}).to_string();
    let mut send = server
        .mount_response(
            |request| request.body_contains_text("send the conditional sibling message"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("late-binding-send"),
                    ev_function_call_with_namespace(
                        "late-send",
                        "multi_agent_v1",
                        "send_input",
                        &args,
                    ),
                    ev_completed("late-binding-send"),
                ]),
            }],
        )
        .await;
    let mut sent = server
        .mount_response(
            |request| request.body_contains_text("late-send"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_assistant_message("late-sent", "accepted"),
                    ev_completed("late-sent"),
                ]),
            }],
        )
        .await;
    let sender_turn = begin(&sender, "send the conditional sibling message").await?;
    timeout(TIMEOUT, send.wait_for_request()).await?;
    timeout(TIMEOUT, sent.wait_for_request()).await?;
    finish(&sender, &sender_turn).await?;
    let submission_key = serde_json::to_string(&(
        "v1-send-input-mailbox",
        sender_id,
        &sender_turn,
        "late-send",
    ))?;
    let accepted = test
        .thread_store
        .lookup_mailbox_input(receiver_id, &submission_key)
        .await?
        .context("accepted sibling mail")?;
    assert_eq!(
        accepted.final_subscription.as_ref().map(|s| s.state),
        Some(MailboxFinalSubscriptionState::Pending)
    );

    // Block every sender proof read, not a sleep or a model-request approximation.
    *store.blocked_sender.lock().expect("arm proof gate") = Some(sender_id);
    let mut bound_response = server
        .mount_response(
            move |request| match binding_source {
                BindingSource::Consumption => {
                    request.body_contains_text("late-check-mail")
                        && request.body_contains_text(PAYLOAD)
                }
                BindingSource::Inventory => {
                    request.body_contains_text("Pending mail snapshot (not consumed):")
                }
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_assistant_message("bound-result", BOUND_FINAL),
                    ev_completed("bound-result"),
                ]),
            }],
        )
        .await;
    release_busy
        .send(())
        .map_err(|_| anyhow::anyhow!("busy response gate closed"))?;
    let bound_turn = if binding_source == BindingSource::Consumption {
        busy_turn
    } else {
        finish(&receiver, &busy_turn).await?;
        assert_eq!(
            receiver
                .try_start_mailbox_inventory_if_idle_with_lease(())
                .await?,
            MailboxInventoryAdmission::Started
        );
        timeout(
            TIMEOUT,
            wait_for_event_match(&receiver, |event| match event {
                EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
                _ => None,
            }),
        )
        .await?
    };
    let request = timeout(TIMEOUT, bound_response.wait_for_request()).await?;
    assert_eq!(
        request.body_contains_text(PAYLOAD),
        binding_source == BindingSource::Consumption
    );
    timeout(TIMEOUT, store.entered.notified())
        .await
        .context("binding must reach the proof gate")?;
    finish(&receiver, &bound_turn).await?;

    let mut next = server
        .mount_response(
            |request| request.body_contains_text("run an unrelated receiver turn before binding"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_assistant_message("later-result", LATER_FINAL),
                    ev_completed("later-result"),
                ]),
            }],
        )
        .await;
    let next_turn = begin(&receiver, "run an unrelated receiver turn before binding").await?;
    assert_ne!(next_turn, bound_turn);
    timeout(TIMEOUT, next.wait_for_request()).await?;
    finish(&receiver, &next_turn).await?;
    let subscription = test
        .thread_store
        .lookup_mailbox_final_subscription(receiver_id, accepted.id.clone())
        .await?
        .context("bound sibling final token")?;
    assert_eq!(
        (subscription.state, subscription.bound_turn_id.as_deref()),
        (
            MailboxFinalSubscriptionState::Bound,
            Some(bound_turn.as_str())
        )
    );
    assert!(
        server
            .requests()
            .await
            .iter()
            .all(|request| !String::from_utf8_lossy(request).contains("<subagent_notification>"))
    );

    let mut wake = server
        .mount_response(
            |request| request.body_contains_text("<subagent_notification>"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_assistant_message("late-observed", "late bound result handled"),
                    ev_completed("late-observed"),
                ]),
            }],
        )
        .await;
    release_proof.send(ProofGateState::Released)?;
    let request = timeout(TIMEOUT, wake.wait_for_request())
        .await
        .context("original final must wake its sibling observer")?;
    assert!(request.body_contains_text(BOUND_FINAL));
    assert!(!request.body_contains_text(LATER_FINAL));
    let wake_turn = timeout(
        TIMEOUT,
        wait_for_event_match(&sender, |event| match event {
            EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
            _ => None,
        }),
    )
    .await?;
    assert_ne!(wake_turn, sender_turn);
    finish(&sender, &wake_turn).await?;
    let delivered = timeout(TIMEOUT, async {
        loop {
            let current = test
                .thread_store
                .lookup_mailbox_final_subscription(receiver_id, accepted.id.clone())
                .await?
                .context("retained final token")?;
            if current.state == MailboxFinalSubscriptionState::Delivered {
                break Ok::<_, anyhow::Error>(current);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await??;
    assert_eq!(
        delivered.bound_turn_id.as_deref(),
        Some(bound_turn.as_str())
    );
    assert_eq!(
        server
            .requests()
            .await
            .iter()
            .filter(|request| String::from_utf8_lossy(request).contains("<subagent_notification>"))
            .count(),
        1
    );
    receiver.shutdown_and_wait().await?;
    sender.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    server.shutdown().await;
    Ok(())
}
