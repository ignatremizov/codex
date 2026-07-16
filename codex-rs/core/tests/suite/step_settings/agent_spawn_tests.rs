//! Spawned agents inherit the invoking step's model settings after active-turn updates.

use super::*;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::protocol::MultiAgentVersion;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(Some(MultiAgentVersion::Disabled), ModelVisibility::List; "disabled catalog tag")]
#[test_case(Some(MultiAgentVersion::V1), ModelVisibility::List; "v1 catalog tag")]
#[test_case(None, ModelVisibility::List; "untagged catalog model")]
#[test_case(Some(MultiAgentVersion::Disabled), ModelVisibility::Hide; "non picker catalog model")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_child_model_keeps_runtime_v2_tools_across_catalog_tags(
    catalog_version: Option<MultiAgentVersion>,
    visibility: ModelVisibility,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spawn"),
                ev_function_call_with_namespace(
                    "spawn-worker",
                    "delegation",
                    "spawn_agent",
                    &json!({
                        "task_name": "catalog-worker",
                        "message": "Complete the delegated task.",
                        "model": MODEL_B,
                        "reasoning_effort": "medium",
                        "fork_turns": "none",
                    })
                    .to_string(),
                ),
                ev_completed("spawn"),
            ]),
            sse_completed("first-completion"),
            sse_completed("second-completion"),
        ],
    )
    .await;
    mount_sse_once(&server, sse_completed("child-notification")).await;
    let test = step_settings_test()
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collab");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable V2");
            config.multi_agent_v2.tool_namespace = Some("delegation".to_string());
            config.multi_agent_v2.expose_spawn_agent_model_overrides = true;
            for model in &mut config.model_catalog.as_mut().expect("catalog").models {
                model.tool_mode = Some(ToolMode::Direct);
                model.use_responses_lite = false;
                model.supports_reasoning_summary_parameter = true;
                if model.slug == MODEL_B {
                    model.multi_agent_version = catalog_version;
                    model.visibility = visibility;
                    model.default_reasoning_level = Some(ReasoningEffort::Medium);
                    model.supported_reasoning_levels = vec![ReasoningEffortPreset {
                        effort: ReasoningEffort::Medium,
                        description: "Balanced".to_string(),
                    }];
                }
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created_threads = test.thread_manager.subscribe_thread_created();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Delegate using the requested model.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let child_id = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 10),
        created_threads.recv(),
    )
    .await??;
    // A completed parent turn does not guarantee the separately spawned child has requested.
    let child_request = tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 10), async {
        loop {
            if let Some(request) = responses
                .requests()
                .into_iter()
                .find(|request| request.header("thread-id") == Some(child_id.to_string()))
            {
                break request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await?;
    assert_eq!(
        request_settings(&child_request),
        json!({
            "model": MODEL_B,
            "reasoning": { "effort": "medium", "summary": "concise" },
            "service_tier": null,
        })
    );
    let body = child_request.body_json();
    for name in ["spawn_agent", "followup_task"] {
        assert!(
            namespace_child_tool(&body, "delegation", name).is_some(),
            "resolved V2 child must retain {name} regardless of its catalog tag"
        );
    }
    let child = test.thread_manager.get_thread(child_id).await?;
    for thread in [child.as_ref(), test.codex.as_ref()] {
        wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_child_model_reports_the_full_catalog_without_creating_a_child() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spawn"),
                ev_function_call_with_namespace(
                    "unknown-model",
                    "collaboration",
                    "spawn_agent",
                    &json!({
                        "task_name": "unknown-model",
                        "message": "This child must not start.",
                        "model": "not-in-catalog",
                        "fork_turns": "none",
                    })
                    .to_string(),
                ),
                ev_completed("spawn"),
            ]),
            sse_completed("rejected"),
        ],
    )
    .await;
    let test = step_settings_test()
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collab");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable V2");
            config.multi_agent_v2.expose_spawn_agent_model_overrides = true;
            let catalog = config.model_catalog.as_mut().expect("catalog");
            let mut hidden = catalog.models[1].clone();
            hidden.visibility = ModelVisibility::Hide;
            hidden.multi_agent_version = Some(MultiAgentVersion::Disabled);
            catalog.models[1] = hidden;
            let template = catalog.models[0].clone();
            for index in 0..6 {
                let mut model = template.clone();
                model.slug = format!("extra-model-{index}");
                catalog.models.push(model);
            }
            for model in &mut catalog.models {
                model.tool_mode = Some(ToolMode::Direct);
                model.use_responses_lite = false;
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let catalog = test
        .thread_manager
        .list_models(RefreshStrategy::Offline, test.config.http_client_factory())
        .await;
    assert!(catalog.len() > 5);
    assert!(
        catalog
            .iter()
            .any(|model| model.model == MODEL_B && !model.show_in_picker)
    );
    let available = catalog
        .iter()
        .map(|model| model.model.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut created_threads = test.thread_manager.subscribe_thread_created();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Try the requested unknown model.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        responses.function_call_output_text("unknown-model"),
        Some(format!(
            "Unknown model `not-in-catalog` for spawn_agent. Available models: {available}"
        ))
    );
    assert_eq!(
        created_threads.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty),
    );
    let root_id = test.session_configured.thread_id.to_string();
    assert!(
        responses
            .requests()
            .iter()
            .all(|request| request.header("thread-id") == Some(root_id.clone()))
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case(false, None; "v1 inherits captured settings")]
#[test_case(true, None; "v2 inherits captured settings")]
#[test_case(false, Some(ReasoningEffort::Medium); "v1 validates effort against captured model")]
#[test_case(true, Some(ReasoningEffort::Medium); "v2 validates effort against captured model")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_inherits_captured_settings_after_a_turn_update(
    multi_agent_v2: bool,
    requested_effort: Option<ReasoningEffort>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut spawn_arguments = json!({ "message": "Complete the delegated task." });
    let namespace = if multi_agent_v2 {
        spawn_arguments["task_name"] = json!("worker");
        spawn_arguments["fork_turns"] = json!("none");
        "collaboration"
    } else {
        "multi_agent_v1"
    };
    if let Some(effort) = &requested_effort {
        spawn_arguments["reasoning_effort"] = json!(effort);
    }
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            paused_response("resp-original", "pause-before-spawn"),
            sse(vec![
                ev_response_created("resp-spawn"),
                ev_function_call_with_namespace(
                    "spawn-worker",
                    namespace,
                    "spawn_agent",
                    &spawn_arguments.to_string(),
                ),
                ev_completed("resp-spawn"),
            ]),
            sse_completed("resp-first-completion"),
            sse_completed("resp-second-completion"),
        ],
    )
    .await;
    // Child completion can trigger another parent request after the parent finishes.
    mount_sse_once(&server, sse_completed("resp-child-notification")).await;
    let test = step_settings_test()
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collab");
            if multi_agent_v2 {
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("enable V2");
            } else {
                config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("disable V2");
            }
            for model in &mut config.model_catalog.as_mut().expect("model catalog").models {
                // An effort-only override must be validated against B, not the turn's initial A.
                model.supported_reasoning_levels = if model.slug == MODEL_B {
                    vec![ReasoningEffort::Medium, ReasoningEffort::High]
                } else {
                    vec![ReasoningEffort::Low]
                }
                .into_iter()
                .map(|effort| ReasoningEffortPreset {
                    description: effort.to_string(),
                    effort,
                })
                .collect();
                model.supports_reasoning_summary_parameter = true;
                model.use_responses_lite = false;
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created_threads = test.thread_manager.subscribe_thread_created();
    let request = start_paused_turn(&test.codex).await?;
    apply_turn_settings(
        &test.codex,
        &request.turn_id,
        TurnSettingsUpdate {
            model: Some(MODEL_B.to_string()),
            effort: Some(Some(ReasoningEffort::High)),
            summary: Some(ReasoningSummary::Detailed),
            ..Default::default()
        },
    )
    .await?;
    answer_paused_turn(&test.codex, &request.turn_id).await?;

    let child_thread_id = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 10),
        created_threads.recv(),
    )
    .await??;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    for thread in [child_thread.as_ref(), test.codex.as_ref()] {
        wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }

    let expected_effort = requested_effort.unwrap_or(ReasoningEffort::High);
    let requests = response_mock.requests();
    let child_request = requests
        .iter()
        .find(|request| request.header("thread-id") == Some(child_thread_id.to_string()))
        .expect("child should make a model request");
    assert_eq!(
        request_settings(child_request),
        json!({
            "model": MODEL_B,
            "reasoning": { "effort": expected_effort, "summary": "detailed" },
            "service_tier": null,
        })
    );

    Ok(())
}
