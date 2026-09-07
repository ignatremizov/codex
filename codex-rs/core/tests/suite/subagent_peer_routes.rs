use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[path = "subagent_downward_permissions.rs"]
mod downward_permissions;

#[path = "subagent_delivery_receipts.rs"]
mod delivery_receipts;

#[test_case(ThreadHistoryMode::Legacy; "non_paginated")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subtree_default_reaches_future_coders_and_pair_overrides_win(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        })
        .build_with_auto_env(&server)
        .await?;
    let supervisor_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let supervisor = test.thread_manager.get_thread(supervisor_id).await?;
    assert_eq!(
        supervisor
            .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
            .await?,
        None
    );
    let sender_id = supervisor
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let recipient_id = supervisor
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let sender = test.thread_manager.get_thread(sender_id).await?;
    let recipient = test.thread_manager.get_thread(recipient_id).await?;
    for (index, permitted) in [true, false, true].into_iter().enumerate() {
        if index > 0 {
            supervisor
                .set_agent_reply_route(
                    &sender_id.to_string(),
                    Some(&recipient_id.to_string()),
                    if permitted {
                        UserAgentReplyRouteMode::Enabled
                    } else {
                        UserAgentReplyRouteMode::Disabled
                    },
                )
                .await?;
        }
        if index == 2 {
            supervisor
                .set_agent_subtree_messaging(UserAgentReplyRouteMode::Disabled)
                .await?;
        }
        let prompt = format!("subtree test {index}");
        let payload = format!("subtree payload {index}");
        let call_id = format!("subtree-call-{index}");
        let prompt_match = prompt.clone();
        let call_match = call_id.clone();
        let invocation = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| {
                body_contains(req, &prompt_match) && !body_contains(req, &call_match)
            },
            sse(vec![
                ev_response_created("subtree-send"),
                ev_function_call_with_namespace(
                    &call_id,
                    MULTI_AGENT_V1_NAMESPACE,
                    "send_input",
                    &serde_json::to_string(
                        &json!({"target":recipient_id.to_string(), "message":payload, "w":"x"}),
                    )?,
                ),
                ev_completed("subtree-send"),
            ]),
        )
        .await;
        let call_match = call_id.clone();
        let output = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| body_contains(req, &call_match),
            sse(vec![
                ev_response_created("subtree-done"),
                ev_assistant_message("subtree-final", "done"),
                ev_completed("subtree-done"),
            ]),
        )
        .await;
        let payload_match = payload.clone();
        let received = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| {
                body_contains(req, "<agent_message>") && body_contains(req, &payload_match)
            },
            sse(vec![
                ev_response_created("subtree-recipient"),
                ev_assistant_message("subtree-received", "received"),
                ev_completed("subtree-recipient"),
            ]),
        )
        .await;
        supervisor
            .prompt_live_agent(
                &sender_id.to_string(),
                vec![UserInput::Text {
                    text: prompt.clone(),
                    text_elements: Vec::new(),
                }],
                UserAgentResponseHandling::Presentation,
            )
            .await?;
        let request = wait_for_request_containing_text(&invocation, &prompt).await?;
        let routes = request
            .message_input_texts("user")
            .into_iter()
            .filter(|text| text.contains("<agent_reply_route>"))
            .collect::<Vec<_>>();
        assert_eq!(
            routes.len(),
            2,
            "supervisor and peer are each discovered once"
        );
        assert!(
            routes
                .iter()
                .any(|text| text.contains(&recipient_id.to_string()))
        );
        assert!(
            !routes
                .iter()
                .any(|text| text.contains(&test.session_configured.thread_id.to_string()))
        );
        let result = wait_for_request_containing_text(&output, &call_id)
            .await?
            .function_call_output(&call_id)
            .to_string();
        wait_for_terminal_status(sender.as_ref()).await?;
        if permitted {
            wait_for_request_containing_text(&received, &payload).await?;
            wait_for_terminal_status(recipient.as_ref()).await?;
            assert!(result.contains("submission_id"), "{result}");
        } else {
            assert!(result.contains("disabled by the user"), "{result}");
            assert!(received.requests().is_empty());
        }
    }
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy, "x"; "non_paginated_direct")]
#[test_case(ThreadHistoryMode::Legacy, "xq"; "non_paginated_queued")]
#[test_case(ThreadHistoryMode::Paginated, "x"; "paginated_direct")]
#[test_case(ThreadHistoryMode::Paginated, "xq"; "paginated_queued")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_grants_peer_route_and_root_only_sees_audit(
    history_mode: ThreadHistoryMode,
    response_flags: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        })
        .build_with_auto_env(&server)
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
    let sender_nickname = sender.config_snapshot().await.session_source.get_nickname();
    let mut received_audits = Vec::new();

    // Exercise the actual model-facing tool before enable, on two enabled turns, and after
    // disable. A grant names a recipient; it does not make Main subscribe to peer replies.
    for (index, enabled) in [false, true, true, false].into_iter().enumerate() {
        if index == 1 || index == 3 {
            let mode = if enabled {
                UserAgentReplyRouteMode::Enabled
            } else {
                UserAgentReplyRouteMode::Disabled
            };
            let outcome = test
                .codex
                .set_agent_reply_route(
                    &sender_id.to_string(),
                    Some(&recipient_id.to_string()),
                    mode,
                )
                .await?;
            assert_eq!((outcome.0, outcome.1), (sender_id, recipient_id));
        }
        let prompt = format!("send peer message on test turn {index}");
        let payload = format!("peer-only contract revision {index}");
        let call_id = format!("peer-call-{index}");
        let prompt_match = prompt.clone();
        let call_match = call_id.clone();
        let invocation = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| {
                body_contains(req, &prompt_match) && !body_contains(req, &call_match)
            },
            sse(vec![
                ev_response_created(&format!("peer-send-{index}")),
                ev_function_call_with_namespace(
                    &call_id,
                    MULTI_AGENT_V1_NAMESPACE,
                    "send_input",
                    &serde_json::to_string(&json!({
                        "target": recipient_id.to_string(),
                        "message": payload,
                        "w": response_flags,
                    }))?,
                ),
                ev_completed(&format!("peer-send-{index}")),
            ]),
        )
        .await;
        let call_match = call_id.clone();
        let tool_result = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| body_contains(req, &call_match),
            sse(vec![
                ev_response_created(&format!("peer-sender-done-{index}")),
                ev_assistant_message(&format!("peer-sender-msg-{index}"), "sender finished"),
                ev_completed(&format!("peer-sender-done-{index}")),
            ]),
        )
        .await;
        let payload_match = payload.clone();
        let receiver_request = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| {
                body_contains(req, "<agent_message>") && body_contains(req, &payload_match)
            },
            sse(vec![
                ev_response_created(&format!("peer-recipient-{index}")),
                ev_assistant_message(&format!("peer-recipient-msg-{index}"), "recipient finished"),
                ev_completed(&format!("peer-recipient-{index}")),
            ]),
        )
        .await;

        test.codex
            .prompt_live_agent(
                &sender_id.to_string(),
                vec![UserInput::Text {
                    text: prompt.clone(),
                    text_elements: Vec::new(),
                }],
                UserAgentResponseHandling::Presentation,
            )
            .await?;
        let sent = wait_for_request_containing_text(&invocation, &prompt).await?;
        assert_eq!(
            sent.message_input_texts("user")
                .iter()
                .filter(|text| text.contains("<agent_reply_route>"))
                .count(),
            usize::from(index > 0),
            "route context is installed once, not on every turn"
        );
        let returned = wait_for_request_containing_text(&tool_result, &call_id).await?;
        let output = returned.function_call_output(&call_id).to_string();
        wait_for_terminal_status(sender.as_ref()).await?;
        if enabled {
            let received = wait_for_request_containing_text(&receiver_request, &payload).await?;
            let envelopes = received
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
                Some(&json!({"nickname": sender_nickname, "ref": "2", "message": payload})),
            );
            let item = wait_for_event_match(recipient.as_ref(), |event| {
                let EventMsg::ItemCompleted(event) = event else {
                    return None;
                };
                match &event.item {
                    TurnItem::AgentMessage(item) if item.attribution.is_some() => {
                        Some(item.clone())
                    }
                    _ => None,
                }
            })
            .await;
            let attribution = item
                .attribution
                .as_ref()
                .expect("trusted recipient attribution");
            assert_eq!(
                (
                    attribution.sender.thread_id,
                    attribution.recipient.thread_id
                ),
                (sender_id, recipient_id),
            );
            assert_eq!(
                json!(attribution.sender_turn_id),
                sent.body_json()["client_metadata"]["turn_id"],
            );
            assert_eq!(
                serde_json::to_value(&item.input)?,
                json!([{"type": "text", "text": payload, "text_elements": []}]),
            );
            received_audits.push(serde_json::to_value(item)?);
            wait_for_terminal_status(recipient.as_ref()).await?;
            assert!(output.contains("submission_id"), "{output}");
        } else {
            let reason = if index == 0 {
                "has no message route"
            } else {
                "disabled by the user"
            };
            assert!(output.contains(reason), "{output}");
            assert!(receiver_request.requests().is_empty());
        }
    }
    assert_eq!(
        test.codex.agent_status().await,
        root_status,
        "audit does not wake Main"
    );
    let mut audits = Vec::new();
    for _ in 0..2 {
        audits.push(
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
        audits, received_audits,
        "Main mirrors the complete trusted recipient item"
    );

    // Main gets live ItemCompleted notifications, not another stored payload. The original
    // received input remains available in the recipient's canonical history in either mode.
    for thread in [test.codex.as_ref(), recipient.as_ref()] {
        thread.flush_rollout().await?;
    }
    let root_history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let recipient_history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: recipient_id,
            include_archived: false,
        })
        .await?;
    let root_json = serde_json::to_string(&root_history.items)?;
    assert!(!root_json.contains("peer-only contract revision"));
    let persisted_audits = recipient_history
        .items
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
        persisted_audits, audits,
        "recipient history retains the complete audit"
    );

    let root_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, "check Main context after peer messaging"),
        sse(vec![
            ev_response_created("peer-root-check"),
            ev_assistant_message("peer-root-check-message", "Main finished"),
            ev_completed("peer-root-check"),
        ]),
    )
    .await;
    test.submit_turn("check Main context after peer messaging")
        .await?;
    let request = root_turn.single_request();
    assert!(
        !request.body_contains_text("peer-only contract revision"),
        "presentation-only copies must not become Main's model input"
    );
    Ok(())
}
