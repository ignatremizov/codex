//! Exact-turn conditional final subscriptions for `send_input` mailbox delivery.

use anyhow::Result;
use codex_core::MailboxInventoryAdmission;
use codex_core::TurnInputRequest;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_extension_api::ThreadIdleCause;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_thread_store::MailboxFinalSubscriptionState;
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
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[test_case(true, false, ThreadHistoryMode::Legacy; "consumed_during_busy_turn_legacy")]
#[test_case(true, false, ThreadHistoryMode::Paginated; "consumed_during_busy_turn_paginated")]
#[test_case(false, false, ThreadHistoryMode::Legacy; "idle_inventory_legacy")]
#[test_case(false, false, ThreadHistoryMode::Paginated; "idle_inventory_paginated")]
#[test_case(true, true, ThreadHistoryMode::Legacy; "consumed_empty_final_legacy")]
#[test_case(true, true, ThreadHistoryMode::Paginated; "consumed_empty_final_paginated")]
#[test_case(false, true, ThreadHistoryMode::Legacy; "inventory_empty_final_legacy")]
#[test_case(false, true, ThreadHistoryMode::Paginated; "inventory_empty_final_paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zf_wakes_exactly_the_turn_that_consumes_or_inventories_accepted_mail(
    consume_during_busy_turn: bool,
    empty_bound_final: bool,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    const PAYLOAD: &str = "private conditional mailbox input";
    const BUSY_FINAL: &str = "busy-turn final without conditional mail";
    const CONSUMING_FINAL: &str = "consuming busy-turn final";
    const INVENTORY_FINAL: &str = "inventory-bound final result";
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
    let receiver_spawn = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Wake,
            ..Default::default()
        })
        .await?;
    let receiver_id = receiver_spawn.target_thread_id;
    let receiver_ref = receiver_spawn
        .agent_ref
        .expect("receiver should have a root-scoped reference")
        .to_string();
    let receiver = test.thread_manager.get_thread(receiver_id).await?;
    test.codex
        .set_agent_reply_route(
            &receiver_id.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;

    let (release_busy_turn, busy_turn_gate) = oneshot::channel();
    let mut busy_turn_response = server
        .mount_response(
            |request| request.body_contains_text("keep receiver busy for zf"),
            vec![StreamingSseChunk {
                gate: Some(busy_turn_gate),
                body: if consume_during_busy_turn {
                    sse(vec![
                        ev_response_created("zf-busy-consume"),
                        ev_function_call_with_namespace(
                            "zf-check-mail",
                            "multi_agent_v1",
                            "check_mail",
                            "{}",
                        ),
                        ev_completed("zf-busy-consume"),
                    ])
                } else {
                    sse(vec![
                        ev_response_created("zf-busy-no-consume"),
                        ev_assistant_message("zf-busy-final", BUSY_FINAL),
                        ev_completed("zf-busy-no-consume"),
                    ])
                },
            }],
        )
        .await;
    let busy_submission = receiver
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "keep receiver busy for zf".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started {
        turn_id: busy_turn_id,
    } = busy_submission
    else {
        anyhow::bail!("receiver should start its busy turn");
    };
    timeout(
        Duration::from_secs(/*secs*/ 15),
        busy_turn_response.wait_for_request(),
    )
    .await?;

    let send_arguments = serde_json::to_string(&json!({
        "target": receiver_id.to_string(),
        "message": PAYLOAD,
        "w": "zf",
    }))?;
    let mut sender_response = server
        .mount_response(
            |request| request.body_contains_text("submit zf while receiver is busy"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("zf-send"),
                    ev_function_call_with_namespace(
                        "zf-send-call",
                        "multi_agent_v1",
                        "send_input",
                        &send_arguments,
                    ),
                    ev_completed("zf-send"),
                ]),
            }],
        )
        .await;
    let mut sender_followup = server
        .mount_response(
            |request| request.body_contains_text("zf-send-call"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("zf-send-followup"),
                    ev_assistant_message("zf-send-done", "mail accepted"),
                    ev_completed("zf-send-followup"),
                ]),
            }],
        )
        .await;
    let sender_submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "submit zf while receiver is busy".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started {
        turn_id: sender_turn_id,
    } = sender_submission
    else {
        anyhow::bail!("sender should start the mailbox submission turn");
    };
    timeout(
        Duration::from_secs(/*secs*/ 15),
        sender_response.wait_for_request(),
    )
    .await?;
    timeout(
        Duration::from_secs(/*secs*/ 15),
        sender_followup.wait_for_request(),
    )
    .await?;
    wait_for_event_match(test.codex.as_ref(), |event| match event {
        EventMsg::TurnComplete(event) if event.turn_id == sender_turn_id => Some(()),
        _ => None,
    })
    .await;
    let submission_key = serde_json::to_string(&(
        "v1-send-input-mailbox",
        test.session_configured.thread_id,
        sender_turn_id,
        "zf-send-call",
    ))?;

    let mut consuming_followup = if consume_during_busy_turn {
        Some(
            server
                .mount_response(
                    |request| {
                        request.body_contains_text("zf-check-mail")
                            && request.body_contains_text(PAYLOAD)
                    },
                    vec![StreamingSseChunk {
                        gate: None,
                        body: if empty_bound_final {
                            sse(vec![
                                ev_response_created("zf-consuming-followup"),
                                ev_completed("zf-consuming-followup"),
                            ])
                        } else {
                            sse(vec![
                                ev_response_created("zf-consuming-followup"),
                                ev_assistant_message("zf-consuming-final", CONSUMING_FINAL),
                                ev_completed("zf-consuming-followup"),
                            ])
                        },
                    }],
                )
                .await,
        )
    } else {
        None
    };
    let mut inventory_response = if consume_during_busy_turn {
        None
    } else {
        Some(
            server
                .mount_response(
                    |request| request.body_contains_text("Pending mail snapshot (not consumed):"),
                    vec![StreamingSseChunk {
                        gate: None,
                        body: if empty_bound_final {
                            sse(vec![
                                ev_response_created("zf-inventory-final"),
                                ev_completed("zf-inventory-final"),
                            ])
                        } else {
                            sse(vec![
                                ev_response_created("zf-inventory-final"),
                                ev_assistant_message("zf-inventory-done", INVENTORY_FINAL),
                                ev_completed("zf-inventory-final"),
                            ])
                        },
                    }],
                )
                .await,
        )
    };
    let expected_final = match (consume_during_busy_turn, empty_bound_final) {
        (true, false) => Some(CONSUMING_FINAL),
        (false, false) => Some(INVENTORY_FINAL),
        (_, true) => None,
    };
    let mut final_wake = server
        .mount_response(
            move |request| {
                request.body_contains_text("<subagent_notification>")
                    && expected_final.is_none_or(|text| request.body_contains_text(text))
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("zf-final-wake"),
                    ev_assistant_message("zf-final-wake-done", "conditional final handled"),
                    ev_completed("zf-final-wake"),
                ]),
            }],
        )
        .await;

    // The sender turn has completed while the receiver remains busy. Its accepted row is still
    // Pending until the receiver consumes it or records the first later idle inventory.
    let accepted = test
        .thread_store
        .lookup_mailbox_input(receiver_id, &submission_key)
        .await?
        .expect("accepted mailbox row");
    assert_eq!(
        accepted
            .final_subscription
            .as_ref()
            .map(|subscription| (subscription.state, subscription.bound_turn_id.as_deref())),
        Some((MailboxFinalSubscriptionState::Pending, None)),
    );

    release_busy_turn
        .send(())
        .map_err(|_| anyhow::anyhow!("receiver's busy response gate should remain open"))?;
    if consume_during_busy_turn {
        let request = timeout(
            Duration::from_secs(/*secs*/ 15),
            consuming_followup
                .as_mut()
                .expect("consuming follow-up mounted")
                .wait_for_request(),
        )
        .await?;
        assert!(request.body_contains_text(PAYLOAD));
    }
    wait_for_event_match(receiver.as_ref(), |event| match event {
        EventMsg::TurnComplete(event) if event.turn_id == busy_turn_id => Some(()),
        _ => None,
    })
    .await;
    receiver
        .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
        .await;

    let expected_bound_turn_id = if consume_during_busy_turn {
        busy_turn_id
    } else {
        let requests = server.requests().await;
        assert!(
            requests.iter().all(|request| {
                !String::from_utf8_lossy(request).contains("<subagent_notification>")
            }),
            "the original ordinary f must be suppressed for an unrelated busy-turn final",
        );
        assert_eq!(
            receiver
                .try_start_mailbox_inventory_if_idle_with_lease(())
                .await?,
            MailboxInventoryAdmission::Started,
        );
        let inventory_response = inventory_response
            .as_mut()
            .expect("idle inventory response mounted");
        let request = timeout(
            Duration::from_secs(/*secs*/ 15),
            inventory_response.wait_for_request(),
        )
        .await?;
        assert!(request.body_contains_text("Pending mail snapshot (not consumed):"));
        assert!(!request.body_contains_text(PAYLOAD));
        wait_for_event_match(receiver.as_ref(), |event| match event {
            EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
            _ => None,
        })
        .await
    };

    if !consume_during_busy_turn {
        wait_for_event_match(receiver.as_ref(), |event| match event {
            EventMsg::TurnComplete(event) if event.turn_id == expected_bound_turn_id => Some(()),
            _ => None,
        })
        .await;
    }
    let subscription = test
        .thread_store
        .lookup_mailbox_input(receiver_id, &submission_key)
        .await?
        .and_then(|accepted| accepted.final_subscription)
        .expect("accepted final subscription");
    assert_eq!(
        subscription.bound_turn_id.as_deref(),
        Some(expected_bound_turn_id.as_str()),
    );
    assert!(matches!(
        subscription.state,
        MailboxFinalSubscriptionState::Bound | MailboxFinalSubscriptionState::Delivered,
    ));

    let wake_request = timeout(
        Duration::from_secs(/*secs*/ 15),
        final_wake.wait_for_request(),
    )
    .await?;
    if empty_bound_final {
        let wake_request_body = wake_request.body_json();
        let notification = wake_request_body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|content| content["text"].as_str())
            .find(|text| text.contains("<subagent_notification>"))
            .expect("empty final wake should contain the structured notification");
        let notification_body = notification
            .strip_prefix("<subagent_notification>")
            .and_then(|body| body.strip_suffix("</subagent_notification>"))
            .expect("notification should have canonical markers");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(notification_body)?,
            json!({
                "ref": receiver_ref,
                "status": {"completed": null},
            }),
        );
    }
    timeout(
        Duration::from_secs(/*secs*/ 15),
        final_wake.wait_for_completion(),
    )
    .await?;
    let notification_requests = server
        .requests()
        .await
        .into_iter()
        .filter(|request| String::from_utf8_lossy(request).contains("<subagent_notification>"))
        .count();
    assert_eq!(notification_requests, 1);
    let subscription = timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let accepted = test
                .thread_store
                .lookup_mailbox_input(receiver_id, &submission_key)
                .await
                .expect("read final mailbox subscription");
            let subscription = accepted.and_then(|accepted| accepted.final_subscription);
            if subscription.as_ref().is_some_and(|subscription| {
                subscription.state == MailboxFinalSubscriptionState::Delivered
            }) {
                break subscription;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    assert_eq!(
        subscription
            .expect("delivered final subscription")
            .bound_turn_id
            .as_deref(),
        Some(expected_bound_turn_id.as_str()),
    );

    server.shutdown().await;
    Ok(())
}
