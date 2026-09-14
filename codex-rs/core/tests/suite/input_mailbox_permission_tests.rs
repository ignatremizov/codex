use super::*;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::models::MessagePhase;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::start_mock_server;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_receipt_is_lean_without_hiding_rejection_or_reordering_delivery(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
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
    let receiver = test.session_configured.thread_id;
    let sender = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Disabled,
        )
        .await?;
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let rejected = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "mixed-rejected".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Rejected private payload.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(sender),
                    recipient: identity(receiver),
                    sender_turn_id: "sender-turn".to_string(),
                }),
            },
            final_subscription: Default::default(),
        })
        .await?;
    let accepted = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "mixed-delivered".to_string(),
            payload: MailboxPayload::User {
                input: vec![UserInput::Text {
                    text: "Delivered user payload.".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
            final_subscription: Default::default(),
        })
        .await?;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("consume-mixed"),
                ev_function_call_with_namespace("mixed-call", "multi_agent_v1", "check_mail", "{}"),
                ev_completed("consume-mixed"),
            ]),
            sse(vec![
                ev_response_created("mixed-done"),
                ev_assistant_message("done", "Delivered mail received; rejection noted."),
                ev_completed("mixed-done"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Check all pending mail.").await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].function_call_output("mixed-call");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(output["output"].as_str().expect("receipt"))?,
        json!({"rejected_count": 1}),
    );
    let input = requests[1].input();
    let result_index = input
        .iter()
        .position(|item| item["type"] == "function_call_output" && item["call_id"] == "mixed-call")
        .expect("tool result");
    let payload_index = input
        .iter()
        .position(|item| item.to_string().contains("Delivered user payload."))
        .expect("delivered input");
    assert!(result_index < payload_index);
    assert!(
        !requests[1]
            .body_json()
            .to_string()
            .contains("Rejected private payload.")
    );
    let stored_rejection = test
        .thread_store
        .lookup_mailbox_input(receiver, &rejected.submission_key)
        .await?
        .expect("rejected mail");
    let stored_delivery = test
        .thread_store
        .lookup_mailbox_input(receiver, &accepted.submission_key)
        .await?
        .expect("delivered mail");
    assert_eq!(
        (stored_rejection.state, stored_delivery.state),
        (MailboxMessageState::Rejected, MailboxMessageState::Consumed),
    );
    let reason = stored_rejection
        .rejection_reason
        .expect("durable rejection details");
    let sender_thread = test.thread_manager.get_thread(sender).await?;
    let warning = wait_for_event(&sender_thread, |event| matches!(event, EventMsg::Warning(warning) if warning.message.contains(&rejected.id))).await;
    assert_eq!(
        serde_json::to_value(warning)?,
        json!({
            "type": "warning",
            "message": format!(
                "Mailbox message {} to {receiver} was rejected: {reason}",
                rejected.id
            ),
        }),
    );
    let history = test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: receiver,
            include_archived: false,
        })
        .await?;
    let canonical = history
        .segments
        .iter()
        .flatten()
        .filter_map(|item| {
            let RolloutItem::ResponseItem(item) = item else {
                return None;
            };
            match &item.item {
                ResponseItem::FunctionCallOutput {
                    call_id: Some(id),
                    output,
                    ..
                } if id == "mixed-call" => Some(
                    serde_json::from_str::<serde_json::Value>(
                        output.text_content().expect("canonical acceptance"),
                    )
                    .expect("JSON"),
                ),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        canonical,
        vec![json!({"status": "delivery_requested", "from": null})]
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_permission_repairs_admitted_context_but_rejects_undelivered_mail() -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::Collab).expect("enable V1");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("disable V2");
    });
    let test = builder
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let receiver = test.session_configured.thread_id;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("mail-author".to_string()),
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let sender = child.target_thread_id;
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    let state = test.codex.state_db().expect("durable graph state");
    let scope = codex_state::AgentSendScope::Directed {
        sender_thread_id: sender,
        receiver_thread_id: receiver,
    };
    let enabled = state.read_agent_send_settings(vec![scope]).await?;
    assert_eq!(enabled[0].mode, codex_state::AgentSendMode::Enabled);
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: Some("StoredAdmissionName".to_string()),
        agent_ref: child.agent_ref.map(|value| value.to_string()),
        task_path: child.task_path.clone(),
        role: Some("reviewer".to_string()),
        model: None,
        reasoning_effort: None,
    };
    let attribution = AgentInputAttribution {
        sender: identity(sender),
        recipient: identity(receiver),
        sender_turn_id: "admission-turn".to_string(),
    };
    let original = vec![UserInput::Text {
        text: "Previously admitted agent context.".to_string(),
        text_elements: Vec::new(),
    }];
    let accepted = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "before-disable".to_string(),
            payload: MailboxPayload::Agent {
                input: original.clone(),
                attribution: Box::new(attribution.clone()),
            },
            final_subscription: Default::default(),
        })
        .await?;
    let arguments = json!({"from": sender.to_string()}).to_string();
    let (release, gate) = oneshot::channel();
    let mut first = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_response_created("recover-admitted"),
                    ev_function_call_with_namespace(
                        "recover-admitted",
                        "multi_agent_v1",
                        "check_mail",
                        &arguments,
                    ),
                    ev_completed("recover-admitted"),
                ]),
            }],
        )
        .await;
    let mut after_repair = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("consume-new"),
                    ev_function_call_with_namespace(
                        "consume-new",
                        "multi_agent_v1",
                        "check_mail",
                        &arguments,
                    ),
                    ev_completed("consume-new"),
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
                    ev_response_created("finished"),
                    ev_assistant_message("finished-message", "Admitted mail recovered."),
                    ev_completed("finished"),
                ]),
            }],
        )
        .await;
    let submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Repair admitted mail and check again.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("expected one receiver turn");
    };
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), first.wait_for_request()).await?;
    let params = ClaimMailboxInputParams {
        invocation: MailboxInvocation {
            receiver_thread_id: receiver,
            turn_id: turn_id.clone(),
            tool_call_id: "recover-admitted".to_string(),
        },
        selection: MailboxSelection::Senders(vec![MailboxSender::Agent(sender)]),
    };
    let claim = test
        .thread_store
        .claim_mailbox_input(params.clone())
        .await?;
    let id = codex_protocol::mailbox_delivery_response_item_id(&claim.messages[0].delivery_id)
        .expect("delivery ID");
    let mut call: ResponseItem = serde_json::from_value(json!({
        "type": "function_call", "call_id": "recover-admitted",
        "name": "check_mail", "namespace": "multi_agent_v1", "arguments": arguments,
    }))?;
    call.set_id(Some(ResponseItemId::new("fc")));
    call.set_turn_id_if_missing(&turn_id);
    let mut result = ResponseItem::from(ResponseInputItem::FunctionCallOutput {
        call_id: "recover-admitted".to_string(),
        output: FunctionCallOutputPayload::from_text(
            json!({"status": "delivery_requested", "from": sender.to_string()}).to_string(),
        ),
    });
    result.set_id(Some(ResponseItemId::new("fco")));
    result.set_turn_id_if_missing(&turn_id);
    let mut context = ResponseItem::from(ResponseInputItem::from_user_input(
        original.clone(),
        LocalImagePreparation::Defer,
    ));
    context.set_id(Some(id.clone()));
    context.set_turn_id_if_missing(&turn_id);
    context.set_create_time_if_missing(serde_json::Number::from(42));
    let context = ResponseItemEnvelope::new(context);
    test.thread_store
        .append_items_and_flush(AppendThreadItemsParams {
            thread_id: receiver,
            items: vec![
                RolloutItem::ResponseItem(call.into()),
                RolloutItem::ResponseItem(result.into()),
                RolloutItem::ResponseItem(context.clone()),
            ],
        })
        .await?;
    let pending = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "undelivered-after-disable".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Undelivered after disable.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(attribution.clone()),
            },
            final_subscription: Default::default(),
        })
        .await?;
    test.codex
        .set_agent_reply_route(
            &sender.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Disabled,
        )
        .await?;
    let disabled = state.read_agent_send_settings(vec![scope]).await?;
    assert_eq!(disabled[0].mode, codex_state::AgentSendMode::Disabled);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("mail response gate closed"))?;
    let repaired_request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        after_repair.wait_for_request(),
    )
    .await?;
    let final_request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        final_response.wait_for_request(),
    )
    .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    for (request, call_id, expected) in [
        (&repaired_request, "recover-admitted", None),
        (&final_request, "recover-admitted", None),
        (
            &final_request,
            "consume-new",
            Some(json!({"status":"rejected", "rejected_count":1})),
        ),
    ] {
        let body = request.body_json();
        let output = body["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
            .expect("mail result");
        let output_text = output["output"].as_str().expect("result");
        if let Some(expected) = expected {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(output_text)?,
                expected
            );
        } else {
            assert_eq!(output_text, "");
        }
    }
    for request in [repaired_request, final_request] {
        let request = request.body_json();
        assert_eq!(
            request["input"]
                .as_array()
                .expect("model input")
                .iter()
                .filter(|item| {
                    item["type"] == "message"
                        && item
                            .to_string()
                            .contains("Previously admitted agent context.")
                })
                .count(),
            1
        );
        assert!(!request.to_string().contains("Undelivered after disable."));
    }
    let repaired = test
        .thread_store
        .claim_mailbox_input(params.clone())
        .await?;
    assert_eq!(
        repaired
            .messages
            .iter()
            .map(|member| (&member.message.id, member.message.state))
            .collect::<Vec<_>>(),
        vec![(&accepted.id, MailboxMessageState::Consumed)],
    );
    let rejected = test
        .thread_store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: MailboxInvocation {
                tool_call_id: "consume-new".to_string(),
                ..params.invocation
            },
            selection: params.selection,
        })
        .await?;
    assert_eq!(
        rejected
            .messages
            .iter()
            .map(|member| (&member.message.id, member.message.state))
            .collect::<Vec<_>>(),
        vec![(&pending.id, MailboxMessageState::Rejected)],
    );
    assert_eq!(state.read_agent_send_settings(vec![scope]).await?, disabled);
    let history = test
        .thread_store
        .load_mailbox_canonical_history(receiver)
        .await?;
    assert_eq!(
        history
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(item) if item.id() == Some(&id) => Some(item.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![context]
    );
    let mut expected = AgentMessageItem::new(&[]);
    let canonical_results = history
        .iter()
        .filter_map(|item| {
            let RolloutItem::ResponseItem(envelope) = item else {
                return None;
            };
            match &envelope.item {
                ResponseItem::FunctionCallOutput {
                    call_id: Some(id),
                    output,
                    ..
                } if id == "recover-admitted" || id == "consume-new" => {
                    Some(serde_json::from_str::<serde_json::Value>(
                        output.text_content().expect("canonical acceptance"),
                    ))
                }
                _ => None,
            }
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        canonical_results,
        vec![json!({"status":"delivery_requested", "from":sender.to_string()}); 2]
    );
    expected.id = id.to_string();
    expected.phase = Some(MessagePhase::Commentary);
    expected.attribution = Some(attribution);
    expected.input = Some(original);
    let presentations = history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.item.id() == id.as_str() =>
            {
                Some(&event.item)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(presentations)?,
        serde_json::to_value(vec![TurnItem::AgentMessage(expected)])?
    );
    test.codex.shutdown_and_wait().await?;
    server.shutdown().await;
    Ok(())
}
