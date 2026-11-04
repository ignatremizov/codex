//! Root recipient receipts follow committed delivery, including between non-root peers.

use super::*;
use codex_protocol::protocol::agent_delivery_receipt_from_response_item_id;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_final_receipt_labels_confirmed_recipient_without_root_context_or_wake(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "delegate work to the other peer";
    const TASK: &str = "inspect the performance harness";
    const ANSWER: &str = "The harness is the bottleneck.";
    const CALL: &str = "delegate-receipt-work";
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
    let observer = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("supervisor".into()),
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let worker = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("scout".into()),
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let observer_thread = test
        .thread_manager
        .get_thread(observer.target_thread_id)
        .await?;
    server
        .mount_response(
            |request| request.body_contains_text(PROMPT) && !request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("peer-delegation"),
                    ev_function_call_with_namespace(
                        CALL,
                        MULTI_AGENT_V1_NAMESPACE,
                        "send_input",
                        &serde_json::to_string(&json!({
                            "target": worker.target_thread_id.to_string(),
                            "message": TASK,
                        }))?,
                    ),
                    ev_completed("peer-delegation"),
                ]),
            }],
        )
        .await;
    server
        .mount_response(
            |request| request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("peer-observer-done"),
                    ev_assistant_message("peer-observer-message", "Delegation admitted."),
                    ev_completed("peer-observer-done"),
                ]),
            }],
        )
        .await;
    let (release, gate) = oneshot::channel();
    let mut response = server
        .mount_response(
            |request| request.body_contains_text(TASK) && !request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_response_created("peer-worker-done"),
                    ev_assistant_message("peer-worker-message", ANSWER),
                    ev_completed("peer-worker-done"),
                ]),
            }],
        )
        .await;
    test.codex
        .prompt_live_agent(
            &observer.target_thread_id.to_string(),
            vec![UserInput::Text {
                text: PROMPT.into(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    let request = wait_for_streaming_request(&mut response).await?;
    let worker_turn = request.body_json()["client_metadata"]["turn_id"]
        .as_str()
        .context("worker turn ID")?
        .to_owned();
    wait_for_terminal_status(observer_thread.as_ref()).await?;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker response gate closed"))?;
    let receipt = wait_for_event_match(test.codex.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        let TurnItem::AgentMessage(item) = &event.item else {
            return None;
        };
        (agent_delivery_receipt_from_response_item_id(&item.id)
            == Some((
                worker.target_thread_id,
                observer.target_thread_id,
                MessagePhase::FinalAnswer,
                SubAgentCompletionModelVisibility::Visible,
            )))
        .then(|| item.clone())
    })
    .await;
    assert_eq!(
        serde_json::to_value(&receipt.content)?,
        json!([{"type": "Text", "text": ""}]),
        "the brief receipt does not retain a second answer payload",
    );
    let labels = receipt.attribution.as_ref().context("receipt labels")?;
    let worker_thread = test
        .thread_manager
        .get_thread(worker.target_thread_id)
        .await?;
    let worker_config = worker_thread.config_snapshot().await;
    let observer_config = observer_thread.config_snapshot().await;
    assert_eq!(
        labels,
        &codex_protocol::AgentInputAttribution {
            sender: codex_protocol::AgentInputIdentity {
                thread_id: worker.target_thread_id,
                nickname: worker.nickname,
                agent_ref: worker.agent_ref.map(|agent_ref| agent_ref.to_string()),
                task_path: worker.task_path,
                role: worker_config.session_source.get_agent_role(),
                model: Some(worker_config.model),
                reasoning_effort: worker_config.reasoning_effort,
            },
            recipient: codex_protocol::AgentInputIdentity {
                thread_id: observer.target_thread_id,
                nickname: observer.nickname,
                agent_ref: observer.agent_ref.map(|agent_ref| agent_ref.to_string()),
                task_path: observer.task_path,
                role: observer_config.session_source.get_agent_role(),
                model: Some(observer_config.model),
                reasoning_effort: observer_config.reasoning_effort,
            },
            sender_turn_id: worker_turn,
            batch_id: None,
        },
    );
    observer_thread.flush_rollout().await?;
    test.codex.flush_rollout().await?;
    for thread_id in [test.session_configured.thread_id, observer.target_thread_id] {
        let history = test
            .thread_store
            .load_canonical_artifact_segments(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?;
        let items = history.segments.iter().flatten().collect::<Vec<_>>();
        assert!(
            !serde_json::to_string(&items)?.contains(&receipt.id),
            "the root receipt is live-only",
        );
        let delivered = items.iter().any(|item| {
            matches!(item, RolloutItem::ResponseItem(item)
                if item.id().is_some_and(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
                    && serde_json::to_string(&item.item).is_ok_and(|text| text.contains(ANSWER)))
        });
        assert_eq!(
            delivered,
            thread_id == observer.target_thread_id,
            "only the confirmed observer receives model context",
        );
    }
    assert_eq!(
        server.requests().await.len(),
        3,
        "receipts do not wake agents"
    );
    server.shutdown().await;
    Ok(())
}
