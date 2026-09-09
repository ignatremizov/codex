use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[path = "subagent_delivery_receipts.rs"]
mod delivery_receipts;

#[test_case(ThreadHistoryMode::Legacy, "x"; "non_paginated_direct")]
#[test_case(ThreadHistoryMode::Legacy, "xq"; "non_paginated_queued")]
#[test_case(ThreadHistoryMode::Paginated, "x"; "paginated_direct")]
#[test_case(ThreadHistoryMode::Paginated, "xq"; "paginated_queued")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_sender_turn_can_repeatedly_send_to_an_idle_same_root_peer(
    history_mode: ThreadHistoryMode,
    response_flags: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "send two peer messages from this one turn";
    const FIRST_CALL: &str = "same-turn-peer-call-1";
    const SECOND_CALL: &str = "same-turn-peer-call-2";
    const FIRST_PAYLOAD: &str = "same-turn peer payload 1";
    const SECOND_PAYLOAD: &str = "same-turn peer payload 2";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
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
    let recipient_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let sender = test.thread_manager.get_thread(sender_id).await?;
    let recipient = test.thread_manager.get_thread(recipient_id).await?;
    let root_status = test.codex.agent_status().await;

    let first_args = serde_json::to_string(&json!({
        "target": recipient_id.to_string(),
        "message": FIRST_PAYLOAD,
        "w": response_flags,
    }))?;
    let mut first_sender_request = server
        .mount_response(
            |request| request.body_contains_text(PROMPT),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("same-turn-peer-send-1"),
                    ev_function_call_with_namespace(
                        FIRST_CALL,
                        MULTI_AGENT_V1_NAMESPACE,
                        "send_input",
                        &first_args,
                    ),
                    ev_completed("same-turn-peer-send-1"),
                ]),
            }],
        )
        .await;
    let second_args = serde_json::to_string(&json!({
        "target": recipient_id.to_string(),
        "message": SECOND_PAYLOAD,
        "w": response_flags,
    }))?;
    let (release_second_send, second_send_gate) = oneshot::channel();
    let mut second_sender_request = server
        .mount_response(
            |request| request.body_contains_text(FIRST_CALL),
            vec![StreamingSseChunk {
                gate: Some(second_send_gate),
                body: sse(vec![
                    ev_response_created("same-turn-peer-send-2"),
                    ev_function_call_with_namespace(
                        SECOND_CALL,
                        MULTI_AGENT_V1_NAMESPACE,
                        "send_input",
                        &second_args,
                    ),
                    ev_completed("same-turn-peer-send-2"),
                ]),
            }],
        )
        .await;
    let mut sender_done = server
        .mount_response(
            |request| request.body_contains_text(SECOND_CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("same-turn-peer-sender-done"),
                    ev_assistant_message("same-turn-peer-sender-final", "both messages sent"),
                    ev_completed("same-turn-peer-sender-done"),
                ]),
            }],
        )
        .await;
    let mut first_recipient_request = server
        .mount_response(
            |request| {
                request.body_contains_text("<agent_message>")
                    && request.body_contains_text(FIRST_PAYLOAD)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("same-turn-peer-recipient-1"),
                    ev_assistant_message("same-turn-peer-recipient-final-1", "received first"),
                    ev_completed("same-turn-peer-recipient-1"),
                ]),
            }],
        )
        .await;
    let mut second_recipient_request = server
        .mount_response(
            |request| {
                request.body_contains_text("<agent_message>")
                    && request.body_contains_text(SECOND_PAYLOAD)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("same-turn-peer-recipient-2"),
                    ev_assistant_message("same-turn-peer-recipient-final-2", "received second"),
                    ev_completed("same-turn-peer-recipient-2"),
                ]),
            }],
        )
        .await;

    test.codex
        .prompt_live_agent(
            &sender_id.to_string(),
            vec![UserInput::Text {
                text: PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    let first_sender_request = wait_for_streaming_request(&mut first_sender_request).await?;
    let sender_turn_id = first_sender_request.body_json()["client_metadata"]["turn_id"].clone();
    let second_sender_request = wait_for_streaming_request(&mut second_sender_request).await?;
    assert_eq!(
        second_sender_request.body_json()["client_metadata"]["turn_id"],
        sender_turn_id,
        "the first tool continuation must remain in the originating sender turn",
    );
    assert_send_result(
        &second_sender_request,
        FIRST_CALL,
        if response_flags.contains('q') {
            "queued"
        } else {
            "submitted"
        },
    )?;

    let first_received = wait_for_streaming_request(&mut first_recipient_request).await?;
    assert_agent_envelope(&first_received, FIRST_PAYLOAD)?;
    let first_audit =
        wait_for_agent_input_audit(recipient.as_ref(), sender_id, recipient_id, FIRST_PAYLOAD)
            .await;
    assert_eq!(
        json!(
            first_audit
                .attribution
                .as_ref()
                .expect("first trusted attribution")
                .sender_turn_id
        ),
        sender_turn_id,
        "the first recipient audit must retain the originating sender turn",
    );
    assert_eq!(
        wait_for_terminal_status(recipient.as_ref()).await?,
        AgentStatus::Completed(Some("received first".to_string())),
        "the first recipient turn must be terminal before the second send is released",
    );

    release_second_send
        .send(())
        .map_err(|_| anyhow::anyhow!("second sender response gate closed"))?;
    let final_sender_request = wait_for_streaming_request(&mut sender_done).await?;
    assert_eq!(
        final_sender_request.body_json()["client_metadata"]["turn_id"],
        sender_turn_id,
        "both peer sends must originate from one sender turn",
    );
    assert_send_result(
        &final_sender_request,
        SECOND_CALL,
        if response_flags.contains('q') {
            "queued"
        } else {
            "submitted"
        },
    )?;
    let second_received = wait_for_streaming_request(&mut second_recipient_request).await?;
    assert_agent_envelope(&second_received, SECOND_PAYLOAD)?;
    let second_audit =
        wait_for_agent_input_audit(recipient.as_ref(), sender_id, recipient_id, SECOND_PAYLOAD)
            .await;
    assert_eq!(
        json!(
            second_audit
                .attribution
                .as_ref()
                .expect("second trusted attribution")
                .sender_turn_id
        ),
        sender_turn_id,
        "the second recipient audit must retain the same originating sender turn",
    );
    assert_eq!(
        wait_for_terminal_status(recipient.as_ref()).await?,
        AgentStatus::Completed(Some("received second".to_string())),
    );
    assert_eq!(
        wait_for_terminal_status(sender.as_ref()).await?,
        AgentStatus::Completed(Some("both messages sent".to_string())),
    );

    assert_eq!(
        test.codex.agent_status().await,
        root_status,
        "peer audits do not wake Main",
    );
    let received_audits = [first_audit, second_audit]
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut root_audits = Vec::new();
    for _ in 0..2 {
        root_audits.push(
            wait_for_event_match(&test.codex, |event| {
                let EventMsg::ItemCompleted(event) = event else {
                    return None;
                };
                let TurnItem::AgentMessage(item) = &event.item else {
                    return None;
                };
                if !item.is_attributed_agent_input_presentation() {
                    return None;
                }
                item.attribution.as_ref()?;
                Some(serde_json::to_value(item).expect("serialize rich audit"))
            })
            .await,
        );
    }
    assert_eq!(
        root_audits, received_audits,
        "Main mirrors both complete trusted recipient items",
    );

    for thread in [test.codex.as_ref(), recipient.as_ref()] {
        thread.flush_rollout().await?;
    }
    let root_history = test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let recipient_history = test
        .thread_store
        .load_mailbox_canonical_history(recipient_id)
        .await?;
    let root_json = serde_json::to_string(&root_history.segments)?;
    for payload in [FIRST_PAYLOAD, SECOND_PAYLOAD] {
        assert!(!root_json.contains(payload));
    }
    let persisted_audits = recipient_history
        .iter()
        .filter_map(|item| {
            let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
                return None;
            };
            match &event.item {
                TurnItem::AgentMessage(item) if item.attribution.is_some() => Some(item),
                _ => None,
            }
        })
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        persisted_audits, root_audits,
        "recipient history retains both complete audits",
    );

    let mut root_turn = server
        .mount_response(
            |request| request.body_contains_text("check Main context after peer messaging"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("same-turn-peer-root-check"),
                    ev_assistant_message("same-turn-peer-root-check-message", "Main finished"),
                    ev_completed("same-turn-peer-root-check"),
                ]),
            }],
        )
        .await;
    test.submit_turn("check Main context after peer messaging")
        .await?;
    let request = wait_for_streaming_request(&mut root_turn).await?;
    for payload in [FIRST_PAYLOAD, SECOND_PAYLOAD] {
        assert!(
            !request.body_contains_text(payload),
            "presentation-only copies must not become Main's model input",
        );
    }
    assert_eq!(
        wait_for_terminal_status(test.codex.as_ref()).await?,
        AgentStatus::Completed(Some("Main finished".to_string())),
    );
    server.shutdown().await;
    Ok(())
}

fn assert_send_result(
    request: &StreamingSseRequest,
    call_id: &str,
    expected_status: &str,
) -> Result<()> {
    let body = request.body_json();
    let output = body["input"]
        .as_array()
        .expect("model input")
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .and_then(|item| item["output"].as_str())
        .ok_or_else(|| anyhow::anyhow!("missing send_input output for {call_id}"))?;
    assert_eq!(
        serde_json::from_str::<Value>(output)?,
        json!({"status": expected_status}),
    );
    Ok(())
}

fn assert_agent_envelope(request: &StreamingSseRequest, expected_message: &str) -> Result<()> {
    let envelopes = request
        .message_input_texts("user")
        .into_iter()
        .filter_map(|text| {
            text.strip_prefix("<agent_message>")?
                .strip_suffix("</agent_message>")
                .map(str::to_owned)
        })
        .map(|body| serde_json::from_str::<Value>(&body))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        envelopes.last(),
        Some(&json!({"ref": "2", "message": expected_message})),
    );
    Ok(())
}

async fn wait_for_agent_input_audit(
    recipient: &codex_core::CodexThread,
    sender_id: ThreadId,
    recipient_id: ThreadId,
    expected_message: &str,
) -> codex_protocol::items::AgentMessageItem {
    wait_for_event_match(recipient, |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        let TurnItem::AgentMessage(item) = &event.item else {
            return None;
        };
        let attribution = item.attribution.as_ref()?;
        let has_expected_message = item.input.as_ref().is_some_and(|input| {
            input.iter().any(
                |input| matches!(input, UserInput::Text { text, .. } if text == expected_message),
            )
        });
        (has_expected_message
            && attribution.sender.thread_id == sender_id
            && attribution.recipient.thread_id == recipient_id)
            .then(|| item.clone())
    })
    .await
}
