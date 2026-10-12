use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_reply_admission_preserves_input_without_route_notice_records(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    const ROOT: &str = "dispatch the scoped-route test";
    const TASK: &str = "receive the scoped assignment";
    const NEXT: &str = "inspect after route revocation";
    const CALL: &str = "scoped-route-send";
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
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let target = child.target_thread_id.to_string();
    let _dispatch = server
        .mount_response(
            |request| request.body_contains_text(ROOT) && !request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("dispatch"),
                    ev_function_call_with_namespace(
                        CALL,
                        MULTI_AGENT_V1_NAMESPACE,
                        "send_input",
                        &serde_json::to_string(
                            &json!({"target": target, "message": TASK, "w": "mx"}),
                        )?,
                    ),
                    ev_completed("dispatch"),
                ]),
            }],
        )
        .await;
    let _dispatched = server
        .mount_response(
            |request| request.body_contains_text(ROOT) && request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("dispatched"),
                    ev_assistant_message("dispatch-done", "dispatched"),
                    ev_completed("dispatched"),
                ]),
            }],
        )
        .await;
    let mut admitted = server
        .mount_response(
            |request| {
                request.body_contains_text(TASK)
                    && !request.body_contains_text(ROOT)
                    && !request.body_contains_text(NEXT)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("admitted"),
                    ev_assistant_message("admitted-done", "received"),
                    ev_completed("admitted"),
                ]),
            }],
        )
        .await;
    let mut revoked = server
        .mount_response(
            |request| request.body_contains_text(NEXT),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("revoked"),
                    ev_assistant_message("revoked-done", "inspected"),
                    ev_completed("revoked"),
                ]),
            }],
        )
        .await;
    test.submit_turn(ROOT).await?;
    let first = wait_for_streaming_request(&mut admitted).await?;
    let child_thread = test
        .thread_manager
        .get_thread(child.target_thread_id)
        .await?;
    wait_for_terminal_status(child_thread.as_ref()).await?;
    test.codex
        .set_agent_reply_route(
            &target,
            /*recipient*/ None,
            UserAgentReplyRouteMode::Disabled,
        )
        .await?;
    test.codex
        .prompt_live_agent(
            &target,
            vec![UserInput::Text {
                text: NEXT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    let second = wait_for_streaming_request(&mut revoked).await?;
    wait_for_terminal_status(child_thread.as_ref()).await?;
    for request in [first, second] {
        assert!(request.body_contains_text(TASK));
        assert!(!request.body_contains_text("<agent_reply_route>"));
    }
    child_thread.flush_rollout().await?;
    let history = test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: child.target_thread_id,
            include_archived: false,
        })
        .await?;
    let routes = history
        .segments
        .iter()
        .flatten()
        .filter_map(|item| {
            let RolloutItem::ResponseItem(envelope) = item else {
                return None;
            };
            let ResponseItem::Message {
                content,
                internal_chat_message_metadata_passthrough: Some(metadata),
                ..
            } = &envelope.item
            else {
                return None;
            };
            content
                .iter()
                .zip(metadata.content_item_kinds.as_ref()?)
                .find_map(|(content, kind)| {
                    let ContentItem::InputText { text } = content else {
                        return None;
                    };
                    (kind.0 == "multi_agent.agent_reply_route").then_some(text)
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(routes, Vec::<&String>::new());
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "non_paginated")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_reply_permission_preserves_an_accepted_user_queue(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const TASK: &str = "execute the accepted prompt after disabling reverse permission";
    const CALL: &str = "accepted-queue-disabled-reply";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let (test, child) = setup_turn_one_with_custom_streamed_child(
        &server,
        json!({"message": CHILD_PROMPT, "w": "x"}),
        gate,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    test.codex
        .set_agent_reply_route(
            &child,
            /*recipient*/ None,
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    let policy = UserAgentResponseHandling::from_parts(
        /*commentary*/ true,
        codex_core::UserAgentFinalResponseHandling::Presentation,
        /*target_messages*/ true,
        /*queue_input*/ true,
    );
    let mut queued_request = server
        .mount_response(
            |request| request.body_contains_text(TASK) && !request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("accepted-queue"),
                    ev_function_call_with_namespace(
                        CALL,
                        MULTI_AGENT_V1_NAMESPACE,
                        "send_input",
                        &serde_json::to_string(&json!({
                            "target": "Main", "message": "must not be forwarded", "w": "x"
                        }))?,
                    ),
                    ev_completed("accepted-queue"),
                ]),
            }],
        )
        .await;
    let mut result_request = server
        .mount_response(
            |request| request.body_contains_text(CALL),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("accepted-queue-result"),
                    ev_assistant_message("accepted-queue-result-message", "accepted work finished"),
                    ev_completed("accepted-queue-result"),
                ]),
            }],
        )
        .await;
    let submission = test
        .codex
        .queue_agent_prompt(
            &child,
            vec![UserInput::Text {
                text: TASK.to_string(),
                text_elements: Vec::new(),
            }],
            policy,
        )
        .await?;
    assert!(submission.queued);
    let before = test.codex.list_user_agent_queued_turns()?;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].response_handling, policy);
    test.codex
        .set_agent_reply_route(
            &child,
            /*recipient*/ None,
            UserAgentReplyRouteMode::Disabled,
        )
        .await?;
    let after = test.codex.list_user_agent_queued_turns()?;
    assert_eq!(after, before);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("child gate closed"))?;
    let request = wait_for_streaming_request(&mut queued_request).await?;
    assert!(request.body_contains_text(TASK));
    assert!(!request.body_contains_text("\"send_input\":\"allowed_this_turn\""));
    let result = wait_for_streaming_request(&mut result_request).await?;
    assert!(result.body_contains_text("disabled by the user"));
    let child_thread = test
        .thread_manager
        .get_thread(ThreadId::from_string(&child)?)
        .await?;
    let _ = wait_for_terminal_status(child_thread.as_ref()).await?;
    assert_eq!(
        test.codex
            .set_agent_reply_route(
                &child,
                /*recipient*/ None,
                UserAgentReplyRouteMode::Disabled
            )
            .await?,
        (
            ThreadId::from_string(&child)?,
            test.session_configured.thread_id,
            Some(UserAgentReplyRouteMode::Disabled)
        ),
    );
    Ok(())
}
