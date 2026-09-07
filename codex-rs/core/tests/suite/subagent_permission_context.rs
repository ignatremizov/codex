use super::*;
use codex_core::UserAgentFinalResponseHandling;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_reverse_grant_disabled_before_eligibility_warns_without_queued_input_or_source_wake(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const QUEUED_PROMPT: &str = "queued task whose reverse grant is revoked";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (child_gate_tx, child_gate_rx) = oneshot::channel();
    // This fixture uses build_with_streaming_server_auto_env, including remote executors.
    let (test, child_id) = setup_turn_one_with_custom_streamed_child(
        &server,
        json!({"message": CHILD_PROMPT, "w": "x"}),
        child_gate_rx,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let source_status = test.codex.agent_status().await;
    let queued = test
        .codex
        .queue_agent_prompt(
            &child_id,
            vec![UserInput::Text {
                text: QUEUED_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::from_parts(
                /*commentary*/ false,
                UserAgentFinalResponseHandling::Presentation,
                /*target_messages*/ true,
                /*queue_input*/ true,
            ),
        )
        .await?;
    assert!(queued.queued);
    let entries = test.codex.list_user_agent_queued_turns();
    assert_eq!(entries.len(), 1);
    let queue_id = entries[0].id.clone();
    test.codex
        .set_agent_reply_route(
            &child_id,
            /*recipient*/ None,
            UserAgentReplyRouteMode::Disabled,
        )
        .await?;
    // The distinct permission notice may continue the already-active child turn.
    // It must not be confused with admission of the queued user input or a source wake.
    let mut permission_notice = server
        .mount_response(
            |request| request.body_contains_text("User disabled send_input"),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("queue-permission-notice"),
                    ev_assistant_message("queue-permission-notice-done", "permission noted"),
                    ev_completed("queue-permission-notice"),
                ]),
            }],
        )
        .await;
    let requests_before = server.requests().await.len();
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let request = wait_for_streaming_request(&mut permission_notice).await?;
    assert!(!request.body_contains_text(QUEUED_PROMPT));
    let warning = wait_for_event_match(test.codex.as_ref(), |event| match event {
        EventMsg::Warning(warning) if warning.message.contains(&queue_id) => Some(warning.clone()),
        _ => None,
    })
    .await;
    assert!(warning.message.contains(&child_id));
    assert!(warning.message.contains("disabled by the user"));
    assert_eq!(server.requests().await.len(), requests_before + 1);
    assert_eq!(test.codex.agent_status().await, source_status);
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_context_is_nonwaking_stable_and_reconciled_after_rollback(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "inspect current messaging context";
    const RECONSTRUCTED_PROMPT: &str = "inspect messaging context after rollback";
    const CALL_ID: &str = "permission-context-self-send";
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
    let initial_status = child.agent_status().await;
    for _ in 0..2 {
        test.codex
            .set_agent_reply_route(
                &child_id.to_string(),
                /*recipient*/ None,
                UserAgentReplyRouteMode::Enabled,
            )
            .await?;
    }
    assert_eq!(child.agent_status().await, initial_status);
    let first = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, PROMPT) && !body_contains(req, CALL_ID),
        sse(vec![
            ev_response_created("permission-first"),
            ev_function_call_with_namespace(
                CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &serde_json::to_string(&json!({
                    "target": child_id.to_string(),
                    "message": "invalid self send",
                    "w": "x",
                }))?,
            ),
            ev_completed("permission-first"),
        ]),
    )
    .await;
    let second = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, CALL_ID),
        sse(vec![
            ev_response_created("permission-second"),
            ev_assistant_message("permission-second-done", "done"),
            ev_completed("permission-second"),
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
    let first_request = wait_for_request_containing_text(&first, PROMPT).await?;
    let second_request = wait_for_request_containing_text(&second, CALL_ID).await?;
    let permission_items = |request: &ResponsesRequest| {
        request
            .input()
            .into_iter()
            .filter(|item| {
                item["role"] == "developer"
                    && item["content"].as_array().is_some_and(|content| {
                        content.iter().any(|item| {
                            item["text"]
                                .as_str()
                                .is_some_and(|text| text.starts_with("User enabled send_input"))
                        })
                    })
            })
            .collect::<Vec<_>>()
    };
    let initial_notices = permission_items(&first_request);
    assert_eq!(initial_notices.len(), 1, "repeated enable is deduplicated");
    assert_eq!(
        permission_items(&second_request),
        initial_notices,
        "unchanged request retains notice ID and metadata"
    );
    wait_for_terminal_status(child.as_ref()).await?;
    let idle_status = child.agent_status().await;
    for _ in 0..2 {
        test.codex
            .set_agent_reply_route(
                &child_id.to_string(),
                /*recipient*/ None,
                UserAgentReplyRouteMode::Disabled,
            )
            .await?;
    }
    assert_eq!(child.agent_status().await, idle_status);
    assert_eq!((first.requests().len(), second.requests().len()), (1, 1));
    child.submit(Op::ThreadRollback { num_turns: 1 }).await?;
    wait_for_event(child.as_ref(), |event| {
        matches!(event, EventMsg::ThreadRolledBack(_))
    })
    .await;
    let reconstructed = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, RECONSTRUCTED_PROMPT),
        sse(vec![
            ev_response_created("permission-reconstructed"),
            ev_assistant_message("permission-reconstructed-done", "done"),
            ev_completed("permission-reconstructed"),
        ]),
    )
    .await;
    test.codex
        .prompt_live_agent(
            &child_id.to_string(),
            vec![UserInput::Text {
                text: RECONSTRUCTED_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    let request = wait_for_request_containing_text(&reconstructed, RECONSTRUCTED_PROMPT).await?;
    let notices = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with("User ") && text.contains("send_input"))
        .collect::<Vec<_>>();
    assert_eq!(notices, vec!["User disabled send_input to Main (1)."]);
    let hints = request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<agent_reply_route>"))
        .map(|text| {
            let body = text
                .strip_prefix("<agent_reply_route>")
                .and_then(|body| body.strip_suffix("</agent_reply_route>"))
                .expect("route envelope");
            serde_json::from_str::<Value>(body).expect("route JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(hints.len(), 1, "route hint remains a singleton");
    assert_eq!(hints[0]["send_input"], json!("not_authorized"));
    wait_for_terminal_status(child.as_ref()).await?;
    Ok(())
}
