use super::*;
use codex_protocol::protocol::agent_delivery_receipt_from_response_item_id;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy, "", SubAgentCompletionModelVisibility::Visible; "legacy_passive")]
#[test_case(ThreadHistoryMode::Paginated, "", SubAgentCompletionModelVisibility::Visible; "paginated_passive")]
#[test_case(ThreadHistoryMode::Legacy, "x", SubAgentCompletionModelVisibility::NotVisible; "legacy_presentation_only")]
#[test_case(ThreadHistoryMode::Paginated, "x", SubAgentCompletionModelVisibility::NotVisible; "paginated_presentation_only")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn main_final_receipt_is_live_only_and_keeps_original_authorship(
    history_mode: ThreadHistoryMode,
    response_handling: &str,
    expected_visibility: SubAgentCompletionModelVisibility,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "ask Main for the receipt contract";
    const MESSAGE: &str = "please provide the receipt contract";
    const ANSWER: &str = "Main owns this final answer.";
    const CALL_ID: &str = "ask-main-receipt";
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
    let child_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let child = test.thread_manager.get_thread(child_id).await?;
    test.codex
        .set_agent_reply_route(
            &child_id.to_string(),
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    let invocation = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, PROMPT) && !body_contains(request, CALL_ID)
        },
        sse(vec![
            ev_response_created("receipt-child"),
            ev_function_call_with_namespace(
                CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &serde_json::to_string(&json!({
                    "target": "Main", "message": MESSAGE, "w": response_handling,
                }))?,
            ),
            ev_completed("receipt-child"),
        ]),
    )
    .await;
    let child_done = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CALL_ID),
        sse(vec![
            ev_response_created("receipt-child-done"),
            ev_assistant_message("receipt-child-message", "Child finished asking."),
            ev_completed("receipt-child-done"),
        ]),
    )
    .await;
    let root_reply = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<agent_message>") && body_contains(request, MESSAGE)
        },
        sse(vec![
            ev_response_created("receipt-root"),
            ev_assistant_message("receipt-root-message", ANSWER),
            ev_completed("receipt-root"),
        ]),
    )
    .await;
    test.codex
        .prompt_live_agent(
            &child_id.to_string(),
            vec![UserInput::Text {
                text: PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    wait_for_request_containing_text(&invocation, PROMPT).await?;
    wait_for_request_containing_text(&child_done, CALL_ID).await?;
    wait_for_request_containing_text(&root_reply, MESSAGE).await?;

    let mut receipts = Vec::new();
    let mut root_child_completion = None;
    wait_for_event(test.codex.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return false;
        };
        let TurnItem::AgentMessage(item) = &event.item else {
            return false;
        };
        if agent_delivery_receipt_from_response_item_id(&item.id).is_some() {
            receipts.push(item.clone());
        }
        if item.has_sub_agent_completion_identity()
            && serde_json::to_string(&item.content)
                .is_ok_and(|text| text.contains("Child finished asking."))
        {
            root_child_completion = Some(item.clone());
        }
        root_child_completion.is_some()
            && (expected_visibility == SubAgentCompletionModelVisibility::NotVisible
                || !receipts.is_empty())
    })
    .await;
    if expected_visibility == SubAgentCompletionModelVisibility::Visible {
        assert_eq!(receipts.len(), 1);
        let receipt = &receipts[0];
        assert_eq!(
            agent_delivery_receipt_from_response_item_id(&receipt.id),
            Some((
                test.session_configured.thread_id,
                child_id,
                MessagePhase::FinalAnswer,
                expected_visibility,
            ))
        );
        assert_eq!(
            serde_json::to_value(&receipt.content)?,
            json!([{"type": "Text", "text": ANSWER}])
        );
        assert!(!receipt.is_attributed_agent_input_presentation());
    } else {
        assert!(receipts.is_empty(), "x must not emit an extra root receipt");
    }
    let completion = wait_for_event_match(child.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        let TurnItem::AgentMessage(item) = &event.item else {
            return None;
        };
        item.has_sub_agent_completion_identity()
            .then(|| item.clone())
    })
    .await;
    assert_eq!(
        serde_json::to_value(&completion.content)?,
        json!([{"type": "Text", "text": format!("Agent final answer from `/root`:\n\n{ANSWER}")}]),
        "child keeps the canonical Main-authored completion"
    );
    assert_eq!(
        sub_agent_completion_model_visibility_from_response_item_id(&completion.id),
        Some(expected_visibility),
    );
    wait_for_terminal_status(child.as_ref()).await?;
    wait_for_terminal_status(test.codex.as_ref()).await?;
    if expected_visibility == SubAgentCompletionModelVisibility::NotVisible {
        // The recipient event is emitted before the independently spawned delivery worker
        // attempts the root mirror. Check its absence beyond that event boundary.
        assert!(
            timeout(
                Duration::from_millis(250),
                wait_for_event_match(test.codex.as_ref(), |event| {
                    let EventMsg::ItemCompleted(event) = event else {
                        return None;
                    };
                    let TurnItem::AgentMessage(item) = &event.item else {
                        return None;
                    };
                    agent_delivery_receipt_from_response_item_id(&item.id)
                }),
            )
            .await
            .is_err(),
            "presentation-only delivery must not mirror a root receipt",
        );
    }
    child.flush_rollout().await?;
    let child_history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: child_id,
            include_archived: false,
        })
        .await?;
    let child_presentations = child_history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.item.id() == completion.id =>
            {
                Some(event.item.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child_presentations,
        vec![TurnItem::AgentMessage(completion)],
        "recipient presentation remains durable for passive and x delivery",
    );
    test.codex.flush_rollout().await?;
    let root_history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let root_json = serde_json::to_string(&root_history.items)?;
    for receipt in &receipts {
        assert!(
            !root_json.contains(&receipt.id),
            "receipt is never persisted"
        );
    }
    let root_child_completion = root_child_completion.expect("ordinary child completion");
    let root_presentations = root_history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.item.id() == root_child_completion.id =>
            {
                Some(event.item.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        root_presentations,
        vec![TurnItem::AgentMessage(root_child_completion)],
        "Main keeps its ordinary durable child-completion oversight",
    );
    assert!(!root_json.contains("Agent final answer from `/root`"));

    let follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "check receipt isolation"),
        sse(vec![
            ev_response_created("receipt-check"),
            ev_assistant_message("receipt-check-message", "Checked."),
            ev_completed("receipt-check"),
        ]),
    )
    .await;
    test.submit_turn("check receipt isolation").await?;
    let request = follow_up.single_request();
    for receipt in &receipts {
        assert!(!request.body_contains_text(&receipt.id));
    }
    assert!(
        !request.body_contains_text("Agent final answer from"),
        "no receipt echo enters Main's model context"
    );
    assert_eq!(
        root_reply.requests().len(),
        1,
        "no receipt-driven Main wake"
    );
    assert_eq!(
        // ResponseMock records before the custom matcher runs, so Main's concurrent
        // request can also be captured here. Count only this child's tool continuation;
        // an unintended subsequent child request would retain the same call in history.
        child_done
            .requests()
            .iter()
            .filter(|request| request.has_function_call(CALL_ID))
            .count(),
        1,
        "receipt cannot start a child turn"
    );
    Ok(())
}
