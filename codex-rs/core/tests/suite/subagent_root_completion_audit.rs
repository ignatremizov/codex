use super::*;
use codex_core::UserAgentObservationMode;
use codex_protocol::protocol::sub_agent_completion_model_visibility_from_response_item_id;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum RootObservation {
    Unobserved,
    Presentation,
    Passive,
}

#[test_case(ThreadHistoryMode::Legacy, RootObservation::Unobserved; "legacy_unobserved")]
#[test_case(ThreadHistoryMode::Paginated, RootObservation::Unobserved; "paginated_unobserved")]
#[test_case(ThreadHistoryMode::Legacy, RootObservation::Presentation; "legacy_root_x")]
#[test_case(ThreadHistoryMode::Paginated, RootObservation::Presentation; "paginated_root_x")]
#[test_case(ThreadHistoryMode::Legacy, RootObservation::Passive; "legacy_root_passive")]
#[test_case(ThreadHistoryMode::Paginated, RootObservation::Passive; "paginated_root_passive")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_completion_has_one_durable_root_row_without_implicit_model_delivery(
    history_mode: ThreadHistoryMode,
    observation: RootObservation,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const SEED: &str = "seed Main before peer work";
    const MAIN_FINAL: &str = "Main's own conclusion";
    const PEER_PROMPT: &str = "delegate the peer audit task";
    const TASK: &str = "perform the peer audit task";
    const CONCLUSION: &str = "peer target audit conclusion";
    const CALL: &str = "peer-audit-send";
    const FOLLOW_UP: &str = "inspect Main after the peer completion";
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
    server
        .mount_response(
            |request| request.body_contains_text(SEED),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("root-seed"),
                    ev_assistant_message("root-seed-final", MAIN_FINAL),
                    ev_completed("root-seed"),
                ]),
            }],
        )
        .await;
    test.submit_turn(SEED).await?;
    let root_status = test.codex.agent_status().await;
    let sender_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let target_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let sender = test.thread_manager.get_thread(sender_id).await?;
    let target = test.thread_manager.get_thread(target_id).await?;
    server
        .mount_response(
            |request| request.body_contains_text("initial target assignment"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("target-initial"),
                    ev_assistant_message("target-initial-final", "initial assignment finished"),
                    ev_completed("target-initial"),
                ]),
            }],
        )
        .await;
    test.codex
        .prompt_live_agent(
            &target_id.to_string(),
            vec![UserInput::Text {
                text: "initial target assignment".to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    wait_for_event_match(test.codex.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        (event.item.is_sub_agent_completion_presentation()
            && serde_json::to_string(&event.item)
                .is_ok_and(|text| text.contains("initial assignment finished")))
        .then_some(())
    })
    .await;
    wait_for_terminal_status(target.as_ref()).await?;
    test.codex
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await?;
    server
        .mount_response(
            |request| request.body_contains_text(PEER_PROMPT) && !request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("peer-send"),
                    ev_function_call_with_namespace(
                        CALL,
                        MULTI_AGENT_V1_NAMESPACE,
                        "send_input",
                        &serde_json::to_string(&json!({
                            "target": target_id.to_string(), "message": TASK, "w": "x",
                        }))?,
                    ),
                    ev_completed("peer-send"),
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
                    ev_response_created("peer-done"),
                    ev_assistant_message("peer-done-final", "peer dispatch finished"),
                    ev_completed("peer-done"),
                ]),
            }],
        )
        .await;
    let (release, gate) = oneshot::channel();
    let mut target_response = server
        .mount_response(
            |request| request.body_contains_text(TASK) && !request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_response_created("target-done"),
                    ev_assistant_message("target-done-final", CONCLUSION),
                    ev_completed("target-done"),
                ]),
            }],
        )
        .await;
    test.codex
        .prompt_live_agent(
            &sender_id.to_string(),
            vec![UserInput::Text {
                text: PEER_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    timeout(Duration::from_secs(10), target_response.wait_for_request()).await?;
    match observation {
        RootObservation::Unobserved => {}
        RootObservation::Presentation | RootObservation::Passive => {
            test.codex
                .observe_agent(
                    &target_id.to_string(),
                    match observation {
                        RootObservation::Presentation => UserAgentObservationMode::Presentation,
                        RootObservation::Passive => UserAgentObservationMode::Passive,
                        RootObservation::Unobserved => unreachable!(),
                    },
                )
                .await?;
        }
    }
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("target gate closed"))?;
    let completion = wait_for_event_match(test.codex.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        let TurnItem::AgentMessage(item) = &event.item else {
            return None;
        };
        (item.has_sub_agent_completion_identity()
            && serde_json::to_string(&item.content).is_ok_and(|text| text.contains(CONCLUSION)))
        .then(|| item.clone())
    })
    .await;
    let expected_visibility = match observation {
        RootObservation::Passive => SubAgentCompletionModelVisibility::Visible,
        RootObservation::Unobserved | RootObservation::Presentation => {
            SubAgentCompletionModelVisibility::NotVisible
        }
    };
    assert_eq!(
        sub_agent_completion_model_visibility_from_response_item_id(&completion.id),
        Some(expected_visibility),
    );
    wait_for_terminal_status(target.as_ref()).await?;
    wait_for_terminal_status(sender.as_ref()).await?;
    assert_eq!(
        test.codex.agent_status().await,
        root_status,
        "audit is not Main's final response"
    );
    assert_eq!(
        server.requests().await.len(),
        5,
        "oversight does not wake Main"
    );
    let mut follow_up = server
        .mount_response(
            |request| request.body_contains_text(FOLLOW_UP),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("root-follow-up"),
                    ev_completed("root-follow-up"),
                ]),
            }],
        )
        .await;
    test.submit_turn(FOLLOW_UP).await?;
    let request = follow_up.wait_for_request().await;
    assert_eq!(
        request.body_contains_text(CONCLUSION),
        matches!(observation, RootObservation::Passive),
        "only Main's explicit observation may deliver the conclusion to its model",
    );
    test.codex.flush_rollout().await?;
    let history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let rows = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => Some(&event.item),
            _ => None,
        })
        .filter(|item| {
            item.is_sub_agent_completion_presentation()
                && serde_json::to_string(item).is_ok_and(|text| text.contains(CONCLUSION))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![&TurnItem::AgentMessage(completion)],
        "canonical replay has one row"
    );
    server.shutdown().await;
    Ok(())
}
