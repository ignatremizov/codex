use super::*;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy, false; "non_paginated")]
#[test_case(ThreadHistoryMode::Paginated, false; "paginated")]
#[test_case(ThreadHistoryMode::Legacy, true; "non_paginated_stale_observation")]
#[test_case(ThreadHistoryMode::Paginated, true; "paginated_stale_observation")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_restores_subtree_enable_and_directed_disable(
    history_mode: ThreadHistoryMode,
    stale_observation: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        });
    let initial = builder.build_with_auto_env(&server).await?;
    initial
        .codex
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await?;
    let left = initial
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let right = initial
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    // Persist an older enabled observation snapshot before the authoritative disable.
    initial
        .codex
        .set_agent_reply_route(
            &left.to_string(),
            Some(&right.to_string()),
            UserAgentReplyRouteMode::Enabled,
        )
        .await?;
    if stale_observation {
        // Model the boundary where authority committed but its observation audit did not.
        initial
            .codex
            .state_db()
            .ok_or_else(|| anyhow::anyhow!("state database"))?
            .replace_agent_send_setting(
                codex_agent_graph_store::AgentSendScope::Directed {
                    sender_thread_id: left,
                    receiver_thread_id: right,
                },
                codex_agent_graph_store::AgentSendMode::Disabled,
            )
            .await?;
    } else {
        initial
            .codex
            .set_agent_reply_route(
                &left.to_string(),
                Some(&right.to_string()),
                UserAgentReplyRouteMode::Disabled,
            )
            .await?;
    }
    initial.codex.flush_rollout().await?;
    let home = Arc::clone(&initial.home);
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("root rollout"))?;
    initial.codex.shutdown_and_wait().await?;
    drop(initial);

    let resumed = builder.resume(&server, home, rollout_path).await?;
    for id in [left, right] {
        resumed
            .codex
            .resume_agent(
                &id.to_string(),
                /*task*/ None,
                UserAgentResponseHandling::Presentation,
            )
            .await?;
    }
    // No setting changes or reauthorization after restart. Downward task dispatch remains
    // available, the explicit left->right disable wins, and right->left inherits Enabled.
    for (index, sender, recipient, permitted) in [(0, left, right, false), (1, right, left, true)] {
        let prompt = format!("persistent-settings-prompt-{index}");
        let call_id = format!("persistent-settings-call-{index}");
        let payload = format!("persistent-settings-payload-{index}");
        let prompt_match = prompt.clone();
        let call_match = call_id.clone();
        let invocation = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| {
                body_contains(req, &prompt_match) && !body_contains(req, &call_match)
            },
            sse(vec![
                ev_response_created("settings-send"),
                ev_function_call_with_namespace(
                    &call_id,
                    MULTI_AGENT_V1_NAMESPACE,
                    "send_input",
                    &serde_json::to_string(
                        &json!({"target":recipient.to_string(),"message":payload,"w":"x"}),
                    )?,
                ),
                ev_completed("settings-send"),
            ]),
        )
        .await;
        let call_match = call_id.clone();
        let output = mount_sse_once_match(
            &server,
            move |req: &wiremock::Request| body_contains(req, &call_match),
            sse(vec![
                ev_response_created("settings-result"),
                ev_assistant_message("settings-result-message", "done"),
                ev_completed("settings-result"),
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
                ev_response_created("settings-received"),
                ev_assistant_message("settings-received-message", "received"),
                ev_completed("settings-received"),
            ]),
        )
        .await;
        resumed
            .codex
            .prompt_live_agent(
                &sender.to_string(),
                vec![UserInput::Text {
                    text: prompt.clone(),
                    text_elements: Vec::new(),
                }],
                UserAgentResponseHandling::Presentation,
            )
            .await?;
        wait_for_request_containing_text(&invocation, &prompt).await?;
        let result = wait_for_request_containing_text(&output, &call_id)
            .await?
            .function_call_output(&call_id)
            .to_string();
        let sender_thread = resumed.thread_manager.get_thread(sender).await?;
        wait_for_terminal_status(sender_thread.as_ref()).await?;
        if permitted {
            assert!(result.contains("submission_id"), "{result}");
            wait_for_request_containing_text(&received, &payload).await?;
            let recipient_thread = resumed.thread_manager.get_thread(recipient).await?;
            wait_for_terminal_status(recipient_thread.as_ref()).await?;
        } else {
            assert!(result.contains("disabled by the user"), "{result}");
            assert!(received.requests().is_empty());
        }
    }
    Ok(())
}
