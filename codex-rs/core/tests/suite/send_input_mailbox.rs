//! V1 send_input admission: durable acceptance is not receiver task admission.
//!
//! These fixtures use test_codex's empty extension registry, without the queue inventory
//! fallback. Exact request counts and unchanged receiver state therefore cover acceptance
//! alone, not a product-wide no-wake guarantee. Scheduler-enabled fixtures must distinguish
//! inventory-only requests from existing receiver turns and forbid mailbox payload injection.

use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::MailboxMessageState;
use codex_thread_store::MailboxPayload;
use codex_thread_store::MailboxSender;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseRequest;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[derive(Clone, Copy)]
enum ReceiverRuntime {
    Idle,
    Unloaded,
    Active,
}

#[test_case("z", ReceiverRuntime::Idle, ThreadHistoryMode::Legacy; "idle")]
#[test_case("zx", ReceiverRuntime::Idle, ThreadHistoryMode::Paginated; "idle_extra_x")]
#[test_case("zfx", ReceiverRuntime::Unloaded, ThreadHistoryMode::Legacy; "unloaded_cancelled_wake")]
#[test_case("zfxx", ReceiverRuntime::Unloaded, ThreadHistoryMode::Paginated; "unloaded_extra_x")]
#[test_case("zz", ReceiverRuntime::Unloaded, ThreadHistoryMode::Legacy; "repeated_z")]
#[test_case("z", ReceiverRuntime::Active, ThreadHistoryMode::Paginated; "active_not_steered")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_accepts_original_typed_input_without_receiver_work(
    flags: &str,
    runtime: ReceiverRuntime,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("not V2");
        })
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let receiver = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let receiver_thread = test.thread_manager.get_thread(receiver).await?;
    receiver_thread.flush_rollout().await?;
    if matches!(runtime, ReceiverRuntime::Unloaded) {
        receiver_thread.shutdown_and_wait().await?;
        test.thread_manager.remove_thread(&receiver).await;
    }
    let release_receiver = if matches!(runtime, ReceiverRuntime::Active) {
        let (release, gate) = oneshot::channel();
        let mut response = server
            .mount_response(
                |request| request.body_contains_text("Keep the receiver busy."),
                vec![StreamingSseChunk {
                    gate: Some(gate),
                    body: sse(vec![
                        ev_response_created("receiver-running"),
                        ev_assistant_message("receiver-final", "Receiver's own conclusion."),
                        ev_completed("receiver-running"),
                    ]),
                }],
            )
            .await;
        receiver_thread
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Keep the receiver busy.".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        timeout(
            Duration::from_secs(/*secs*/ 15),
            response.wait_for_request(),
        )
        .await?;
        Some(release)
    } else {
        None
    };
    if matches!(runtime, ReceiverRuntime::Active) {
        receiver_thread.flush_rollout().await?;
    }
    let receiver_status = receiver_thread.agent_status().await;
    let history_params = LoadThreadHistoryParams {
        thread_id: receiver,
        include_archived: false,
    };
    let receiver_history = test
        .thread_store
        .load_rollback_history(history_params.clone())
        .await?;
    let input = vec![
        UserInput::Text {
            text: "inspect [image]".to_string(),
            text_elements: vec![TextElement::new(
                ByteRange { start: 8, end: 15 },
                Some("original image span".to_string()),
            )],
        },
        UserInput::Image {
            image_url: "data:image/png;base64,original-mailbox-bytes".to_string(),
            detail: None,
        },
        UserInput::Mention {
            name: "original connector".to_string(),
            path: "app://mailbox-fixture".to_string(),
        },
    ];
    server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("mail-send"),
                    ev_function_call_with_namespace(
                        "mail-accept",
                        "multi_agent_v1",
                        "send_input",
                        &serde_json::to_string(&json!({
                            "target": receiver.to_string(), "items": input, "w": flags,
                        }))?,
                    ),
                    ev_completed("mail-send"),
                ]),
            }],
        )
        .await;
    let mut followup = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("mail-accepted"),
                    ev_assistant_message("mail-accepted-final", "Accepted only."),
                    ev_completed("mail-accepted"),
                ]),
            }],
        )
        .await;
    let submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Deposit the attachment without starting receiver work.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("sender should start a new turn");
    };
    let started = wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::ItemStarted(event)
            if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == "mail-accept"))
    })
    .await;
    let completed = wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::ItemCompleted(event)
            if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == "mail-accept"))
    })
    .await;
    let EventMsg::ItemStarted(started) = started else {
        anyhow::bail!("expected send_input start");
    };
    let EventMsg::ItemCompleted(completed) = completed else {
        anyhow::bail!("expected send_input completion");
    };
    for (item, status) in [
        (started.item, CollabAgentToolCallStatus::InProgress),
        (completed.item, CollabAgentToolCallStatus::Completed),
    ] {
        let TurnItem::CollabAgentToolCall(call) = item else {
            anyhow::bail!("expected structured send_input item");
        };
        assert_eq!(
            (
                call.status,
                call.mailbox_input,
                call.observe_commentary,
                call.wake_on_completion,
                call.target_messages,
                call.queue_input,
                call.agents_states,
            ),
            (
                status,
                Some(true),
                Some(false),
                None,
                Some(false),
                Some(false),
                Default::default(),
            ),
        );
    }
    wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = timeout(
        Duration::from_secs(/*secs*/ 15),
        followup.wait_for_request(),
    )
    .await?;
    let result: Value = serde_json::from_str(&call_output(&request, "mail-accept")?)?;
    let key = serde_json::to_string(&(
        "v1-send-input-mailbox",
        test.session_configured.thread_id,
        &turn_id,
        "mail-accept",
    ))?;
    let accepted = test
        .thread_store
        .lookup_mailbox_input(receiver, &key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("accepted mailbox row"))?;
    assert_eq!(result, json!({"status": "mailboxAccepted"}));
    assert_eq!(accepted.state, MailboxMessageState::Pending);
    assert_eq!(
        accepted.sender,
        MailboxSender::Agent(test.session_configured.thread_id)
    );
    let MailboxPayload::Agent {
        input: stored_input,
        attribution,
    } = accepted.payload
    else {
        anyhow::bail!("agent submission must retain agent authorship");
    };
    assert_eq!(stored_input, input);
    assert_eq!(
        (
            attribution.sender.thread_id,
            attribution.recipient.thread_id,
            attribution.sender_turn_id
        ),
        (test.session_configured.thread_id, receiver, turn_id),
    );
    match runtime {
        ReceiverRuntime::Idle | ReceiverRuntime::Active => {
            assert_eq!(receiver_thread.agent_status().await, receiver_status)
        }
        ReceiverRuntime::Unloaded => {
            assert!(test.thread_manager.get_thread(receiver).await.is_err());
            assert_eq!(
                (
                    attribution.recipient.model,
                    attribution.recipient.reasoning_effort
                ),
                (None, None)
            );
        }
    }
    assert_eq!(
        serde_json::to_value(
            test.thread_store
                .load_rollback_history(history_params)
                .await?
                .items,
        )?,
        serde_json::to_value(receiver_history.items)?,
        "acceptance did not inject receiver context or observations",
    );
    if let Some(release) = release_receiver {
        assert_eq!(receiver_thread.agent_status().await, AgentStatus::Running);
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("receiver gate closed"))?;
        wait_for_event(receiver_thread.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    let expected_requests = if matches!(runtime, ReceiverRuntime::Active) {
        3
    } else {
        2
    };
    assert_eq!(
        server.requests().await.len(),
        expected_requests,
        "no mailbox task was admitted"
    );
    server.shutdown().await;
    Ok(())
}

#[test_case("zc", false; "commentary")]
#[test_case("zm", false; "transient_reply_grant")]
#[test_case("zq", false; "queued_turn")]
#[test_case("zffx", false; "remaining_wake")]
#[test_case("z", true; "interrupt")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_mailbox_flags_never_admit_receiver_input(
    flags: &str,
    interrupt: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("not V2");
        })
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let receiver = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let target = test.thread_manager.get_thread(receiver).await?;
    let before = target.agent_status().await;
    server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("invalid-mail"),
                    ev_function_call_with_namespace(
                        "invalid-mail-call",
                        "multi_agent_v1",
                        "send_input",
                        &serde_json::to_string(&json!({
                            "target": receiver.to_string(), "message": "must not arrive",
                            "w": flags, "interrupt": interrupt,
                        }))?,
                    ),
                    ev_completed("invalid-mail"),
                ]),
            }],
        )
        .await;
    let mut followup = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("invalid-done"),
                    ev_completed("invalid-done"),
                ]),
            }],
        )
        .await;
    test.submit_turn("Try the incompatible mailbox flags.")
        .await?;
    let request = timeout(
        Duration::from_secs(/*secs*/ 15),
        followup.wait_for_request(),
    )
    .await?;
    assert!(call_output(&request, "invalid-mail-call")?.contains("mailbox"));
    assert_eq!(target.agent_status().await, before);
    assert_eq!(server.requests().await.len(), 2);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_retry_reuses_accepted_attribution_after_unload_and_revocation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("not V2");
        })
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let sender = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let receiver = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let source = test.thread_manager.get_thread(sender).await?;
    let target = test.thread_manager.get_thread(receiver).await?;
    target.flush_rollout().await?;
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            Some(&receiver.to_string()),
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    let input = vec![
        UserInput::Text {
            text: "immutable accepted message".to_string(),
            text_elements: Vec::new(),
        },
        UserInput::Image {
            image_url: "data:image/png;base64,original-mailbox-bytes".to_string(),
            detail: None,
        },
    ];
    let arguments = serde_json::to_string(&json!({
        "target": receiver.to_string(), "items": input, "w": "z",
    }))?;
    server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("initial-mail"),
                    ev_function_call_with_namespace(
                        "retry-mail",
                        "multi_agent_v1",
                        "send_input",
                        &arguments,
                    ),
                    ev_completed("initial-mail"),
                ]),
            }],
        )
        .await;
    let (release, gate) = oneshot::channel();
    let mut retry = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_response_created("retry-mail-response"),
                    ev_function_call_with_namespace(
                        "retry-mail",
                        "multi_agent_v1",
                        "send_input",
                        &arguments,
                    ),
                    ev_completed("retry-mail-response"),
                ]),
            }],
        )
        .await;
    let mut changed = server.mount_response(
        |_| true,
        vec![StreamingSseChunk { gate: None, body: sse(vec![
            ev_response_created("changed-mail"),
            ev_function_call_with_namespace("retry-mail", "multi_agent_v1", "send_input",
                &serde_json::to_string(&json!({
                    "target": receiver.to_string(), "message": "changed retry payload", "w": "z",
                }))?,
            ),
            ev_completed("changed-mail"),
        ]) }],
    ).await;
    let mut denied = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("new-mail"),
                    ev_function_call_with_namespace(
                        "new-mail-call",
                        "multi_agent_v1",
                        "send_input",
                        &arguments,
                    ),
                    ev_completed("new-mail"),
                ]),
            }],
        )
        .await;
    let mut final_response = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("retry-done"),
                    ev_completed("retry-done"),
                ]),
            }],
        )
        .await;
    let submission = source
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Deposit mail, retry the same invocation, then make a new send.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("sender should start a new turn");
    };
    let first_request = timeout(Duration::from_secs(/*secs*/ 15), retry.wait_for_request()).await?;
    let first_output = call_output(&first_request, "retry-mail")?;
    let key = serde_json::to_string(&("v1-send-input-mailbox", sender, &turn_id, "retry-mail"))?;
    let first = test
        .thread_store
        .lookup_mailbox_input(receiver, &key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("initial accepted row"))?;
    let receipt_id = codex_protocol::mailbox_acceptance_receipt_id(&first.id)
        .ok_or_else(|| anyhow::anyhow!("acceptance identity"))?;
    let receipt = wait_for_event_match(test.codex.as_ref(), |event| match event {
        EventMsg::ItemCompleted(event) if event.item.id() == receipt_id.as_str() => {
            Some(event.item.clone())
        }
        _ => None,
    })
    .await;
    let MailboxPayload::Agent { attribution, input } = &first.payload else {
        anyhow::bail!("expected stored agent mail");
    };
    let mut expected_receipt = codex_protocol::items::AgentMessageItem::new(&[]);
    expected_receipt.id = receipt_id.to_string();
    expected_receipt.phase = Some(codex_protocol::models::MessagePhase::Commentary);
    expected_receipt.attribution = Some(attribution.as_ref().clone());
    expected_receipt.input = Some(input.clone());
    assert_eq!(receipt, TurnItem::AgentMessage(expected_receipt));
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            Some(&receiver.to_string()),
            UserAgentReplyRouteMode::Disabled,
        )
        .await?;
    target.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&receiver).await;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("retry gate closed"))?;
    let retry_request =
        timeout(Duration::from_secs(/*secs*/ 15), changed.wait_for_request()).await?;
    assert_eq!(call_output(&retry_request, "retry-mail")?, first_output);
    let changed_request =
        timeout(Duration::from_secs(/*secs*/ 15), denied.wait_for_request()).await?;
    assert!(call_output(&changed_request, "retry-mail")?.contains("different input"));
    let final_request = timeout(
        Duration::from_secs(/*secs*/ 15),
        final_response.wait_for_request(),
    )
    .await?;
    assert!(call_output(&final_request, "new-mail-call")?.contains("configured"));
    wait_for_event(source.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        test.thread_store
            .lookup_mailbox_input(receiver, &key)
            .await?,
        Some(first)
    );
    let new_key =
        serde_json::to_string(&("v1-send-input-mailbox", sender, &turn_id, "new-mail-call"))?;
    assert_eq!(
        test.thread_store
            .lookup_mailbox_input(receiver, &new_key)
            .await?,
        None
    );
    assert!(test.thread_manager.get_thread(receiver).await.is_err());
    // All acceptance attempts have returned: drain queued Main events to reject retry or
    // inventory notices without assuming parent completion synchronizes child work.
    while let Ok(event) = timeout(
        Duration::from_millis(/*millis*/ 100),
        test.codex.next_event(),
    )
    .await
    {
        if let EventMsg::ItemCompleted(event) = event?.msg {
            assert!(!codex_protocol::is_mailbox_acceptance_receipt_id(
                &event.item.id()
            ));
        }
    }
    let root_history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    assert!(
        !serde_json::to_string(&root_history.items)?.contains(receipt_id.as_str()),
        "the live notice must not enter Main's canonical history or model context",
    );
    assert_eq!(server.requests().await.len(), 5);
    server.shutdown().await;
    Ok(())
}

#[derive(Clone, Copy)]
enum SendAuthority {
    UnconfiguredPeer,
    ConfiguredSubtree,
    DirectedDisable,
    ForeignRoot,
}

#[test_case(SendAuthority::UnconfiguredPeer; "unconfigured_peer")]
#[test_case(SendAuthority::ConfiguredSubtree; "configured_subtree")]
#[test_case(SendAuthority::DirectedDisable; "directed_disable_overrides_subtree")]
#[test_case(SendAuthority::ForeignRoot; "foreign_uuid_without_adoption")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_acceptance_uses_durable_same_root_send_authority(
    authority: SendAuthority,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("not V2");
        })
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let sender = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let receiver = match authority {
        SendAuthority::ForeignRoot => {
            test.thread_manager
                .start_thread(StartThreadOptions::new(test.config.clone()))
                .await?
                .thread_id
        }
        SendAuthority::UnconfiguredPeer
        | SendAuthority::ConfiguredSubtree
        | SendAuthority::DirectedDisable => {
            test.codex
                .spawn_agent(UserAgentSpawnOptions::default())
                .await?
                .target_thread_id
        }
    };
    let source = test.thread_manager.get_thread(sender).await?;
    let target = test.thread_manager.get_thread(receiver).await?;
    target.flush_rollout().await?;
    if matches!(
        authority,
        SendAuthority::ConfiguredSubtree | SendAuthority::DirectedDisable
    ) {
        test.codex
            .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
            .await?;
    }
    if matches!(authority, SendAuthority::DirectedDisable) {
        test.codex
            .set_agent_reply_route(
                &sender.to_string(),
                Some(&receiver.to_string()),
                UserAgentReplyRouteMode::Disabled,
            )
            .await?;
    }
    server.mount_response(
        |_| true,
        vec![StreamingSseChunk { gate: None, body: sse(vec![
            ev_response_created("mail-authority"),
            ev_function_call_with_namespace(
                "authority-call", "multi_agent_v1", "send_input",
                &serde_json::to_string(&json!({
                    "target": receiver.to_string(), "message": "authority checked mail", "w": "z",
                }))?,
            ),
            ev_completed("mail-authority"),
        ]) }],
    ).await;
    let mut followup = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("authority-done"),
                    ev_completed("authority-done"),
                ]),
            }],
        )
        .await;
    let before = target.agent_status().await;
    let submission = source
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Try one mailbox deposit.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("sender should start a new turn");
    };
    let request = timeout(
        Duration::from_secs(/*secs*/ 15),
        followup.wait_for_request(),
    )
    .await?;
    let result = call_output(&request, "authority-call")?;
    wait_for_event(source.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let key = serde_json::to_string(&("v1-send-input-mailbox", sender, turn_id, "authority-call"))?;
    let accepted = test
        .thread_store
        .lookup_mailbox_input(receiver, &key)
        .await?;
    match authority {
        SendAuthority::ConfiguredSubtree => {
            assert!(accepted.is_some(), "configured mail accepted");
            assert_eq!(
                serde_json::from_str::<Value>(&result)?,
                json!({"status": "mailboxAccepted"})
            );
        }
        SendAuthority::UnconfiguredPeer | SendAuthority::DirectedDisable => {
            assert!(result.contains("configured"));
            assert_eq!(accepted, None);
        }
        SendAuthority::ForeignRoot => {
            assert!(result.contains("cross-root"));
            assert_eq!(accepted, None);
        }
    }
    assert_eq!(target.agent_status().await, before);
    assert_eq!(server.requests().await.len(), 2);
    server.shutdown().await;
    Ok(())
}

fn call_output(request: &StreamingSseRequest, call_id: &str) -> Result<String> {
    request.body_json()["input"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .rev()
                .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        })
        .and_then(|item| item["output"].as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing tool result for {call_id}"))
}
