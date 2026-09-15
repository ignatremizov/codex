//! Array targets exercise real admission, canonical deduplication, and per-recipient results.

use super::*;
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
