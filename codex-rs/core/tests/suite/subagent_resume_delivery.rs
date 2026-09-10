use super::*;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentObservationBinding;
use codex_protocol::error::CodexErrorDetails;
use core_test_support::responses::mount_sse_once;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_wake_is_bound_before_prompt_observation_publication(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
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
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Wake,
            ..Default::default()
        })
        .await?;
    let child_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "run the reserved observation task")
                && !body_contains(request, "<subagent_notification>")
        },
        sse(vec![
            ev_response_created("reserved-observation-child"),
            ev_assistant_message("reserved-observation-final", "reserved observation result"),
            ev_completed("reserved-observation-child"),
        ]),
    )
    .await;
    let wake_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_notification>")
                && body_contains(request, "reserved observation result")
        },
        sse(vec![
            ev_response_created("reserved-observation-parent"),
            ev_assistant_message("reserved-observation-parent-final", "received"),
            ev_completed("reserved-observation-parent"),
        ]),
    )
    .await;
    let admitted = test
        .codex
        .prompt_live_agent(
            &child.target_thread_id.to_string(),
            vec![UserInput::Text {
                text: "run the reserved observation task".to_owned(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    assert_eq!(
        admitted,
        codex_core::UserAgentPromptResult {
            target_thread_id: child.target_thread_id,
            submission_id: admitted.submission_id.clone(),
            queued: false,
            input_outcome: codex_core::UserAgentInputOutcome::Admitted,
            resumed_target: false,
            post_admission_warning: None,
        },
    );
    let request =
        wait_for_request_containing_text(&wake_response, "reserved observation result").await?;
    assert!(request.body_contains_text("<subagent_notification>"));
    child_response.single_request();
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unloaded_agent_resumes_at_capacity_without_reallocating_its_registration(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
            config.agent_max_threads = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    let handling = UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*target_messages*/ false,
        /*queue_input*/ false,
    );
    let options = UserAgentSpawnOptions {
        response_handling: handling,
        ..Default::default()
    };
    let spawned = test.codex.spawn_agent(options.clone()).await?;
    let child = test
        .thread_manager
        .get_thread(spawned.target_thread_id)
        .await?;
    let original_source = child.config_snapshot().await.session_source;
    child.ensure_rollout_materialized().await;
    child.flush_rollout().await?;
    child.shutdown_and_wait().await?;
    child.wait_until_terminated().await;
    assert!(
        test.thread_manager
            .remove_thread(&spawned.target_thread_id)
            .await
            .is_some()
    );

    let resumed = test
        .codex
        .resume_agent(
            &spawned.target_thread_id.to_string(),
            /*task*/ None,
            handling,
        )
        .await?;
    assert_eq!(
        (
            resumed.target_thread_id,
            resumed.agent_ref,
            resumed.nickname,
            resumed.task_path,
        ),
        (
            spawned.target_thread_id,
            spawned.agent_ref,
            spawned.nickname,
            spawned.task_path,
        ),
    );
    let restored = test
        .thread_manager
        .get_thread(spawned.target_thread_id)
        .await?;
    assert!(!Arc::ptr_eq(&child, &restored));
    assert_eq!(
        restored.config_snapshot().await.session_source,
        original_source
    );
    let error = test
        .codex
        .spawn_agent(options.clone())
        .await
        .err()
        .context("the retained child still consumes exactly one slot")?;
    assert!(matches!(
        error.details(),
        CodexErrorDetails::AgentLimitReached { max_threads: 1 }
    ));
    test.codex
        .close_agent(&spawned.target_thread_id.to_string(), handling)
        .await?;
    let replacement = test.codex.spawn_agent(options).await?;
    assert_ne!(replacement.target_thread_id, spawned.target_thread_id);
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy, false; "legacy_idle")]
#[test_case(ThreadHistoryMode::Paginated, false; "paginated_idle")]
#[test_case(ThreadHistoryMode::Legacy, true; "legacy_completed")]
#[test_case(ThreadHistoryMode::Paginated, true; "paginated_completed")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_presentation_resume_reserves_next_turn_and_preserves_prior_wake(
    history_mode: ThreadHistoryMode,
    complete_child: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
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
            response_handling: UserAgentResponseHandling::from_parts(
                /*commentary*/ false,
                UserAgentFinalResponseHandling::None,
                /*target_messages*/ false,
                /*queue_input*/ false,
            ),
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let child = test.thread_manager.get_thread(child_id).await?;
    if complete_child {
        let child_response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("status-only-child"),
                ev_assistant_message("status-only-final", "already finished"),
                ev_completed("status-only-child"),
            ]),
        )
        .await;
        child
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "finish before the status query".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        assert!(matches!(
            wait_for_terminal_status(child.as_ref()).await?,
            AgentStatus::Completed(_)
        ));
        assert!(
            child_response
                .single_request()
                .body_contains_text("finish before the status query")
        );
    }

    let selector = child_id.to_string();
    for queue_input in [false, true] {
        let result = test
            .codex
            .resume_agent(
                &selector,
                /*task*/ None,
                UserAgentResponseHandling::from_parts(
                    /*commentary*/ false,
                    UserAgentFinalResponseHandling::Presentation,
                    /*target_messages*/ false,
                    queue_input,
                ),
            )
            .await?;
        assert_eq!(
            result.observation_binding,
            Some(UserAgentObservationBinding::NextTurn),
        );
    }
    let history = test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let audits = history
        .segments
        .iter()
        .flatten()
        .filter_map(|item| match item {
            RolloutItem::AgentResponseObservation(observation)
                if observation.target_thread_id == child_id =>
            {
                Some(observation)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        audits.len() >= 3,
        "spawn and both status queries must be audited"
    );
    assert!(audits.iter().all(|audit| {
        audit.target_turn_id.is_none()
            && !audit.pending_commentary
            && matches!(
                audit.final_delivery,
                AgentResponseFinalDelivery::None | AgentResponseFinalDelivery::PresentationOnly
            )
            && audit.final_delivery_response_item_id.is_none()
    }));
    assert_eq!(
        audits.last().map(|audit| audit.final_delivery),
        Some(AgentResponseFinalDelivery::PresentationOnly),
    );

    let subscribed = test
        .codex
        .resume_agent(
            &selector,
            /*task*/ None,
            UserAgentResponseHandling::Wake,
        )
        .await?;
    assert_eq!(
        subscribed.observation_binding,
        Some(UserAgentObservationBinding::NextTurn)
    );
    let queried = test
        .codex
        .resume_agent(
            &selector,
            /*task*/ None,
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    assert_eq!(queried.observation_binding, subscribed.observation_binding);
    let history = test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let latest = history
        .segments
        .iter()
        .flatten()
        .rev()
        .find_map(|item| match item {
            RolloutItem::AgentResponseObservation(observation)
                if observation.target_thread_id == child_id =>
            {
                Some(observation)
            }
            _ => None,
        })
        .context("retained pending wake")?;
    assert_eq!(
        (latest.target_turn_id.as_ref(), latest.final_delivery),
        (None, AgentResponseFinalDelivery::Wake)
    );
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_does_not_redeliver_visible_child_turn(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
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
        .with_history_mode(history_mode)
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
    // Reopening creates an idle runtime with no admitted turn, not a replay of its
    // historical completion. The model-facing resume result separately renders this as idle.
    assert_eq!(
        resumed.agents_states,
        HashMap::from([(child_id, AgentStatus::PendingInit)]),
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
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let child_reference = child.to_string();
    let committed_finals = history
        .segments
        .iter()
        .flatten()
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
    for item in history.segments.iter().flatten() {
        match item {
            RolloutItem::ResponseItem(item)
                if item.id().is_some_and(|id| committed_finals.contains(id))
                    && matches!(&item.item, ResponseItem::AgentMessage { .. }) =>
            {
                model += 1;
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
