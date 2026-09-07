use super::*;
use codex_protocol::protocol::agent_delivery_receipt_from_response_item_id;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "non_paginated")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn main_final_receipt_is_live_only_and_keeps_original_authorship(
    history_mode: ThreadHistoryMode,
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
                    "target": "Main", "message": MESSAGE, "w": "x",
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

    let receipt = wait_for_event_match(test.codex.as_ref(), |event| {
        let EventMsg::ItemCompleted(event) = event else {
            return None;
        };
        let TurnItem::AgentMessage(item) = &event.item else {
            return None;
        };
        agent_delivery_receipt_from_response_item_id(&item.id).map(|_| item.clone())
    })
    .await;
    assert_eq!(
        agent_delivery_receipt_from_response_item_id(&receipt.id),
        Some((
            test.session_configured.thread_id,
            child_id,
            MessagePhase::FinalAnswer,
            SubAgentCompletionModelVisibility::NotVisible,
        ))
    );
    assert_eq!(
        serde_json::to_value(&receipt.content)?,
        json!([{"type": "Text", "text": ANSWER}])
    );
    assert!(!receipt.is_attributed_agent_input_presentation());
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
    wait_for_terminal_status(child.as_ref()).await?;
    wait_for_terminal_status(test.codex.as_ref()).await?;
    test.codex.flush_rollout().await?;
    let root_history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let root_json = serde_json::to_string(&root_history.items)?;
    assert!(
        !root_json.contains(&receipt.id),
        "receipt is never persisted"
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
    assert!(!request.body_contains_text(&receipt.id));
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
        child_done.requests().len(),
        1,
        "receipt cannot start a child turn"
    );
    Ok(())
}
