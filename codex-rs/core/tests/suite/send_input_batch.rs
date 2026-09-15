//! Array targets exercise real admission, canonical deduplication, and per-recipient results.

use super::*;
use codex_history::RolloutItem as HistoryRolloutItem;
use codex_protocol::CollabAgentInputStatus;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case("z", false, false; "plain")]
#[test_case("zf", true, false; "conditional")]
#[test_case("zfx", false, false; "cancelled_final_flag")]
#[test_case("z", false, true; "busy_receiver_not_steered")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_batch_keeps_receiver_scoped_acceptance_and_partial_results(
    flags: &str,
    expects_subscription: bool,
    busy_receiver: bool,
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
    let first = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let second = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    for receiver in [first, second] {
        test.thread_manager
            .get_thread(receiver)
            .await?
            .flush_rollout()
            .await?;
    }
    let second_thread = test.thread_manager.get_thread(second).await?;
    second_thread.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&second).await;
    let first_thread = test.thread_manager.get_thread(first).await?;
    let release_receiver = if busy_receiver {
        let (release, gate) = oneshot::channel();
        let mut running = server
            .mount_response(
                |request| request.body_contains_text("Keep batch receiver busy."),
                vec![StreamingSseChunk {
                    gate: Some(gate),
                    body: sse(vec![
                        ev_response_created("receiver-running"),
                        ev_assistant_message("receiver-final", "Receiver finished."),
                        ev_completed("receiver-running"),
                    ]),
                }],
            )
            .await;
        first_thread
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Keep batch receiver busy.".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        timeout(Duration::from_secs(/*secs*/ 15), running.wait_for_request()).await?;
        Some(release)
    } else {
        None
    };
    let first_status = first_thread.agent_status().await;
    let sender = test.session_configured.thread_id;
    server.mount_response(
        |_| true,
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("batch-send"),
                ev_function_call_with_namespace(
                    "batch-call", "multi_agent_v1", "send_input",
                    &serde_json::to_string(&json!({
                        "target": [first.to_string(), second.to_string(), format!("id:{first}"), "missing-selector", sender.to_string()],
                        "items": [{"type":"text","text":"Shared mailbox message."}],
                        "w": flags,
                    }))?,
                ),
                ev_completed("batch-send"),
            ]),
        }],
    ).await;
    let mut followup = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("batch-result"),
                    ev_assistant_message("batch-final", "Results received."),
                    ev_completed("batch-result"),
                ]),
            }],
        )
        .await;
    let submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Send the mailbox batch.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("sender should start");
    };
    let event = wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::ItemCompleted(event)
            if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == "batch-call"))
    }).await;
    let EventMsg::ItemCompleted(event) = event else {
        anyhow::bail!("completed item");
    };
    let TurnItem::CollabAgentToolCall(call) = event.item else {
        anyhow::bail!("collab item");
    };
    let batch = call
        .input_batch
        .ok_or_else(|| anyhow::anyhow!("batch results"))?;
    assert_eq!(
        (
            call.status,
            call.receiver_thread_ids,
            call.agents_states,
            batch.flags
        ),
        (
            CollabAgentToolCallStatus::Failed,
            vec![first, second, sender],
            Default::default(),
            if expects_subscription {
                "zf".to_string()
            } else {
                "z".to_string()
            },
        ),
    );
    assert_eq!(
        batch
            .results
            .iter()
            .map(|result| (
                result.receiver_thread_id.clone(),
                result.status,
                result.error.is_some(),
                result.hint.is_some(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                Some(first.to_string()),
                CollabAgentInputStatus::MailboxAccepted,
                false,
                false
            ),
            (
                Some(second.to_string()),
                CollabAgentInputStatus::MailboxAccepted,
                false,
                true
            ),
            (None, CollabAgentInputStatus::Error, true, false),
            (
                Some(sender.to_string()),
                CollabAgentInputStatus::Error,
                true,
                false
            ),
        ],
    );
    wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = timeout(
        Duration::from_secs(/*secs*/ 15),
        followup.wait_for_request(),
    )
    .await?;
    let output: Value = serde_json::from_str(&call_output(&request, "batch-call")?)?;
    let expected = batch
        .results
        .iter()
        .map(|result| {
            let mut entry = json!({"target": result.target, "status": result.status});
            if let Some(error) = &result.error {
                entry["error"] = json!(error);
            }
            if let Some(hint) = &result.hint {
                entry["hint"] = json!(hint);
            }
            entry
        })
        .collect::<Vec<_>>();
    assert_eq!(output, json!({"results": expected}));
    let key = serde_json::to_string(&("v1-send-input-mailbox", sender, &turn_id, "batch-call"))?;
    let mut ids = Vec::new();
    for receiver in [first, second] {
        let accepted = test
            .thread_store
            .lookup_mailbox_input(receiver, &key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("receiver-scoped acceptance"))?;
        assert_eq!(
            (accepted.state, accepted.final_subscription.is_some()),
            (MailboxMessageState::Pending, expects_subscription)
        );
        let MailboxPayload::Agent { input, attribution } = accepted.payload else {
            anyhow::bail!("attributed agent payload");
        };
        assert_eq!(
            (
                input,
                attribution.sender.thread_id,
                attribution.recipient.thread_id,
                attribution.sender_turn_id
            ),
            (
                vec![UserInput::Text {
                    text: "Shared mailbox message.".into(),
                    text_elements: Vec::new()
                }],
                sender,
                receiver,
                turn_id.clone()
            ),
        );
        ids.push(accepted.id);
    }
    assert_ne!(ids[0], ids[1]);
    assert!(test.thread_manager.get_thread(second).await.is_err());
    assert_eq!(first_thread.agent_status().await, first_status);
    if let Some(release) = release_receiver {
        let _ = release.send(());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_batch_mirrors_one_root_aggregate_and_keeps_canonical_boundaries() -> Result<()> {
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
    let sender_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let first_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let second_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let sender = test.thread_manager.get_thread(sender_id).await?;
    let first = test.thread_manager.get_thread(first_id).await?;
    let second = test.thread_manager.get_thread(second_id).await?;
    for thread in [&sender, &first, &second] {
        thread.flush_rollout().await?;
    }
    for receiver_id in [first_id, second_id] {
        test.codex
            .set_agent_reply_route(
                &sender_id.to_string(),
                Some(&receiver_id.to_string()),
                UserAgentReplyRouteMode::Enabled,
            )
            .await?;
    }

    let call_id = "child-batch-call";
    let shared_body = "Shared child batch body.";
    let child_prompt = "Ask both peers to review the shared child batch.";
    let first_body_match = shared_body.to_string();
    let mut first_receiver = server
        .mount_response(
            move |request: &StreamingSseRequest| {
                request.body_contains_text(&first_body_match)
                    && !request.body_contains_text(call_id)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("first-recipient"),
                    ev_assistant_message("first-recipient-final", "First finished."),
                    ev_completed("first-recipient"),
                ]),
            }],
        )
        .await;
    let second_body_match = shared_body.to_string();
    let mut second_receiver = server
        .mount_response(
            move |request: &StreamingSseRequest| {
                request.body_contains_text(&second_body_match)
                    && !request.body_contains_text(call_id)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("second-recipient"),
                    ev_assistant_message("second-recipient-final", "Second finished."),
                    ev_completed("second-recipient"),
                ]),
            }],
        )
        .await;
    let child_prompt_match = child_prompt.to_string();
    let mut child_invocation = server
        .mount_response(
            move |request: &StreamingSseRequest| request.body_contains_text(&child_prompt_match),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("child-send"),
                    ev_function_call_with_namespace(
                        call_id,
                        "multi_agent_v1",
                        "send_input",
                        &serde_json::to_string(&json!({
                            "target": [first_id.to_string(), second_id.to_string(), "missing-selector"],
                            "message": shared_body,
                            "w": "x",
                        }))?,
                    ),
                    ev_completed("child-send"),
                ]),
            }],
        )
        .await;
    let mut child_followup = server
        .mount_response(
            move |request: &StreamingSseRequest| request.body_contains_text(call_id),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("child-followup"),
                    ev_assistant_message("child-followup-final", "Child finished."),
                    ev_completed("child-followup"),
                ]),
            }],
        )
        .await;

    test.codex
        .prompt_live_agent(
            &sender_id.to_string(),
            vec![UserInput::Text {
                text: child_prompt.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    timeout(
        Duration::from_secs(/*secs*/ 15),
        child_invocation.wait_for_request(),
    )
    .await?;
    timeout(
        Duration::from_secs(/*secs*/ 15),
        child_followup.wait_for_request(),
    )
    .await?;
    timeout(
        Duration::from_secs(/*secs*/ 15),
        first_receiver.wait_for_request(),
    )
    .await?;
    timeout(
        Duration::from_secs(/*secs*/ 15),
        second_receiver.wait_for_request(),
    )
    .await?;

    let sender_call = wait_for_event_match(sender.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        let TurnItem::CollabAgentToolCall(call) = &event.item else {
            return None;
        };
        (call.id == call_id).then(|| call.clone())
    })
    .await;
    let root_call = loop {
        let event = timeout(Duration::from_secs(/*secs*/ 15), test.codex.next_event()).await??;
        let EventMsg::ItemCompleted(event) = event.msg else {
            continue;
        };
        match event.item {
            TurnItem::AgentMessage(item) if item.is_attributed_agent_input_presentation() => {
                anyhow::bail!("root received an ungrouped recipient audit: {item:?}");
            }
            TurnItem::CollabAgentToolCall(call) if call.id.contains(call_id) => break call,
            _ => {}
        }
    };
    assert_eq!(
        root_call.id,
        format!("agent-input-batch/{sender_id}/{call_id}")
    );
    let mut expected_root_call = sender_call.clone();
    expected_root_call.id = root_call.id.clone();
    assert_eq!(root_call, expected_root_call);
    let sender_batch = sender_call
        .input_batch
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("sender batch lifecycle"))?;
    assert_eq!(sender_batch.sender_thread_id, Some(sender_id));
    assert_eq!(
        sender_batch
            .results
            .iter()
            .map(|result| (result.receiver_thread_id.clone(), result.status))
            .collect::<Vec<_>>(),
        vec![
            (
                Some(first_id.to_string()),
                CollabAgentInputStatus::Submitted
            ),
            (
                Some(second_id.to_string()),
                CollabAgentInputStatus::Submitted
            ),
            (None, CollabAgentInputStatus::Error),
        ],
    );

    for thread in [&sender, &first, &second] {
        wait_for_event(thread.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        thread.flush_rollout().await?;
    }
    for _ in 0..test.codex.queued_event_count() {
        let Some(event) = test.codex.try_next_event()? else {
            break;
        };
        if let EventMsg::ItemCompleted(event) = event.msg {
            match event.item {
                TurnItem::AgentMessage(item) if item.is_attributed_agent_input_presentation() => {
                    anyhow::bail!("root received a late ungrouped recipient audit: {item:?}");
                }
                TurnItem::CollabAgentToolCall(call) if call.id == root_call.id => {
                    anyhow::bail!("root received a duplicate batch aggregate");
                }
                _ => {}
            }
        }
    }
    let mut recipient_attributions = Vec::new();
    for receiver_id in [first_id, second_id] {
        let history = test
            .thread_store
            .load_mailbox_canonical_history(receiver_id)
            .await?;
        let items = history
            .iter()
            .filter_map(|item| {
                let HistoryRolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
                    return None;
                };
                let TurnItem::AgentMessage(message) = &event.item else {
                    return None;
                };
                (message
                    .attribution
                    .as_ref()
                    .is_some_and(|attribution| attribution.batch_id.as_deref() == Some(call_id)))
                .then(|| message.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 1);
        let item = items.into_iter().next().expect("one recipient item");
        let attribution = item.attribution.as_ref().expect("recipient attribution");
        assert_eq!(
            (
                attribution.sender.thread_id,
                attribution.recipient.thread_id,
                attribution.batch_id.as_deref(),
            ),
            (sender_id, receiver_id, Some(call_id)),
        );
        assert_eq!(
            serde_json::to_value(&item.input)?,
            json!([{"type": "text", "text": shared_body, "text_elements": []}]),
        );
        recipient_attributions.push(serde_json::to_value(&item)?);
    }
    assert_ne!(recipient_attributions[0], recipient_attributions[1]);

    test.codex.flush_rollout().await?;
    let sender_history = test
        .thread_store
        .load_history(LoadThreadHistoryParams {
            thread_id: sender_id,
            include_archived: false,
        })
        .await?;
    let root_history = test
        .thread_store
        .load_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let sender_json = serde_json::to_string(&sender_history.items)?;
    let root_json = serde_json::to_string(&root_history.items)?;
    assert!(sender_json.contains(call_id));
    assert!(sender_json.contains(shared_body));
    assert!(!root_json.contains(call_id));
    assert!(!root_json.contains(shared_body));

    let root_prompt_match = "Check Main after child batch.".to_string();
    let mut root_check = server
        .mount_response(
            move |request: &StreamingSseRequest| request.body_contains_text(&root_prompt_match),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("root-check"),
                    ev_assistant_message("root-check-final", "Main finished."),
                    ev_completed("root-check"),
                ]),
            }],
        )
        .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Check Main after child batch.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let root_request = timeout(
        Duration::from_secs(/*secs*/ 15),
        root_check.wait_for_request(),
    )
    .await?;
    assert!(!root_request.body_contains_text(shared_body));
    Ok(())
}
