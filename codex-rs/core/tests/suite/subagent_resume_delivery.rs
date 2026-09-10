use super::*;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_does_not_redeliver_visible_child_turn() -> Result<()> {
    let final_text = "same final";
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "seed resume receipt test"),
        sse(vec![
            ev_response_created("seed-resume"),
            ev_assistant_message("seed-resume-final", "ready"),
            ev_completed("seed-resume"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collab");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed resume receipt test").await?;
    let child_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions::default())
        .await?
        .target_thread_id;
    let child = test.thread_manager.get_thread(child_id).await?;
    for (prompt, response_id, message_id) in [
        ("first child assignment", "child-first", "child-first-final"),
        ("later child assignment", "child-later", "child-later-final"),
    ] {
        mount_sse_once_match(
            &server,
            move |request: &wiremock::Request| body_contains(request, prompt),
            sse(vec![
                ev_response_created(response_id),
                ev_assistant_message(message_id, final_text),
                ev_completed(response_id),
            ]),
        )
        .await;
    }
    test.codex
        .prompt_live_agent(
            &child_id.to_string(),
            vec![UserInput::Text {
                text: "first child assignment".to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Passive,
        )
        .await?;
    let _ = wait_for_terminal_status(child.as_ref()).await?;
    // Wait for the independently running observer, not just the child's model turn.
    wait_for_event_match(test.codex.as_ref(), sub_agent_completion_event).await;

    let close_args = json!({"target": "2"});
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "close and resume child"),
        sse(vec![
            ev_response_created("close-child"),
            ev_function_call_with_namespace(
                "close-receipt",
                MULTI_AGENT_V1_NAMESPACE,
                "close_agent",
                &close_args.to_string(),
            ),
            ev_completed("close-child"),
        ]),
    )
    .await;
    let close_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "close-receipt"),
        sse(vec![
            ev_response_created("resume-child"),
            ev_function_call_with_namespace(
                "resume-receipt",
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                r#"{"id":"2"}"#,
            ),
            ev_completed("resume-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "resume-receipt"),
        sse(vec![
            ev_response_created("after-resume"),
            ev_assistant_message("after-resume-final", "resumed"),
            ev_completed("after-resume"),
        ]),
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "close and resume child".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let resumed = wait_for_event_match(test.codex.as_ref(), |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::CollabAgentToolCall(item) if item.id == "resume-receipt" => {
                Some(item.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(
        resumed.agents_states,
        HashMap::from([(child_id, AgentStatus::Completed(None))]),
    );
    wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let close_outputs = close_followup
        .requests()
        .iter()
        .filter_map(|request| request.function_call_output_text("close-receipt"))
        .collect::<Vec<_>>();
    assert_eq!(close_outputs.len(), 1);
    let output: Value = serde_json::from_str(&close_outputs[0])?;
    assert_eq!(output["response_delivery"], json!("already_visible"));
    wait_for_final_counts(&test, child_id, /*expected*/ (1, 1)).await?;

    test.codex
        .prompt_live_agent(
            &child_id.to_string(),
            vec![UserInput::Text {
                text: "later child assignment".to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Passive,
        )
        .await?;
    wait_for_final_counts(&test, child_id, /*expected*/ (2, 2)).await?;
    // Finish observer work before the final exact assertion. A duplicate from resume or a
    // payload-based rejection of the later identical final changes these canonical counts.
    let child = test.thread_manager.get_thread(child_id).await?;
    let _ = wait_for_terminal_status(child.as_ref()).await?;
    assert_eq!(final_counts(&test, child_id).await?, (2, 2));
    Ok(())
}

async fn wait_for_final_counts(
    test: &TestCodex,
    child: ThreadId,
    expected: (usize, usize),
) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            let actual = final_counts(test, child).await?;
            if actual.0 >= expected.0 && actual.1 >= expected.1 {
                assert_eq!(actual, expected);
                return Ok::<(), anyhow::Error>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn final_counts(test: &TestCodex, child: ThreadId) -> Result<(usize, usize)> {
    let history = test
        .codex
        .read_thread(
            /*include_archived*/ false, /*include_history*/ true,
        )
        .await?
        .history
        .expect("canonical parent history");
    let child_reference = child.to_string();
    let committed_finals = history
        .items
        .iter()
        .filter_map(|item| {
            let RolloutItem::AgentResponseObservation(observation) = item else {
                return None;
            };
            let final_id = observation.final_delivery_response_item_id.as_ref()?;
            (observation.observer_thread_id == test.session_configured.thread_id
                && observation.target_thread_id == child
                && observation
                    .committed_delivery_response_item_ids
                    .contains(final_id))
            .then_some(final_id)
        })
        .collect::<HashSet<_>>();
    let mut model = 0;
    let mut visible = 0;
    for item in &history.items {
        match item {
            RolloutItem::ResponseItem(item) => {
                if item.id().is_some_and(|id| committed_finals.contains(id))
                    && matches!(&item.item, ResponseItem::AgentMessage { .. })
                {
                    model += 1;
                }
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                if let Some((id, _, reference, _)) = sub_agent_completion_item(&event.item)
                    && reference == child_reference
                    && sub_agent_completion_model_visibility_from_response_item_id(&id)
                        == Some(SubAgentCompletionModelVisibility::Visible)
                {
                    visible += 1;
                }
            }
            _ => {}
        }
    }
    Ok((model, visible))
}
