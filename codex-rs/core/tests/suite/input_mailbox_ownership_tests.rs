use super::*;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_thread_store::RejectMailboxInputParams;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_receipt_hides_rejected_cross_root_mail_without_hiding_same_root_delivery(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
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
    let sender = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let foreign = ThreadId::new();
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
            submission_key: "mixed-cross-root".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Rejected cross-root payload.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(foreign),
                    recipient: identity(receiver),
                    sender_turn_id: "foreign-turn".to_string(),
                    batch_id: None,
                }),
            },
            final_subscription: Default::default(),
        })
        .await?;
    let accepted = test
        .thread_store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: receiver,
            submission_key: "mixed-same-root".to_string(),
            payload: MailboxPayload::Agent {
                input: vec![UserInput::Text {
                    text: "Delivered same-root payload.".to_string(),
                    text_elements: Vec::new(),
                }],
                attribution: Box::new(AgentInputAttribution {
                    sender: identity(sender),
                    recipient: identity(receiver),
                    sender_turn_id: "same-root-turn".to_string(),
                    batch_id: None,
                }),
            },
            final_subscription: Default::default(),
        })
        .await?;
    let (release, gate) = oneshot::channel();
    let mut first = server
        .mount_response(
            |request| request.body_contains_text("Check all pending mail."),
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_response_created("consume-mixed"),
                    ev_function_call_with_namespace(
                        "mixed-call",
                        "multi_agent_v1",
                        "check_mail",
                        "{}",
                    ),
                    ev_completed("consume-mixed"),
                ]),
            }],
        )
        .await;
    let mut final_response = server
        .mount_response(
            |request| request.body_contains_text("mixed-call"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("mixed-done"),
                    ev_assistant_message(
                        "done",
                        "Delivered same-root mail; rejected cross-root mail.",
                    ),
                    ev_completed("mixed-done"),
                ]),
            }],
        )
        .await;
    let submission = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Check all pending mail.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submission else {
        anyhow::bail!("expected a receiver turn");
    };
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), first.wait_for_request()).await?;
    let claim = test
        .thread_store
        .claim_mailbox_input(ClaimMailboxInputParams {
            invocation: MailboxInvocation {
                receiver_thread_id: receiver,
                turn_id,
                tool_call_id: "mixed-call".to_string(),
            },
            selection: MailboxSelection::All,
        })
        .await?;
    assert_eq!(claim.messages.len(), 2);
    test.thread_store
        .reject_mailbox_input(RejectMailboxInputParams {
            receiver_thread_id: receiver,
            message_id: rejected.id.clone(),
            reason: "cross-root agent mail is isolated".to_string(),
        })
        .await?;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("mail response gate closed"))?;
    let request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        final_response.wait_for_request(),
    )
    .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let body = request.body_json();
    let input = body["input"].as_array().expect("model input");
    let output = input
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "mixed-call")
        .expect("receipt");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(output["output"].as_str().expect("output"))?,
        json!({"rejected_count": 1}),
    );
    let result_index = input
        .iter()
        .position(|item| item["type"] == "function_call_output" && item["call_id"] == "mixed-call")
        .expect("tool result");
    let payload_index = input
        .iter()
        .position(|item| item.to_string().contains("Delivered same-root payload."))
        .expect("delivered input");
    assert!(result_index < payload_index);
    assert!(!body.to_string().contains("Rejected cross-root payload."));
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
    assert!(
        stored_rejection
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("cross-root"))
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
    server.shutdown().await;
    Ok(())
}
