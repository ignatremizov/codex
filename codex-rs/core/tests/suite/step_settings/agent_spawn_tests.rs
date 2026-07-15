//! Children inherit captured model settings and persistent environment attachments.

use super::*;
use codex_core::config::MultiAgentMessageDelivery;
use codex_exec_server::CreateDirectoryOptions;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::protocol::TurnEnvironmentSelection;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::environment_config_for_selection;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum EnvironmentInheritanceCase {
    Configured,
    Legacy,
    Empty,
    Fork,
    Pending,
}

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
                        "task_name": "catalog_worker",
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
            config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::Encrypted;
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
                        "task_name": "unknown_model",
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
            config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::Encrypted;
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

#[test_case(false, None, EnvironmentInheritanceCase::Configured; "v1 inherits captured settings")]
#[test_case(true, None, EnvironmentInheritanceCase::Configured; "v2 inherits captured settings")]
#[test_case(false, Some(ReasoningEffort::Medium), EnvironmentInheritanceCase::Configured; "v1 validates effort against captured model")]
#[test_case(true, Some(ReasoningEffort::Medium), EnvironmentInheritanceCase::Configured; "v2 validates effort against captured model")]
#[test_case(false, None, EnvironmentInheritanceCase::Empty; "v1 excludes turn-only attachments")]
#[test_case(true, None, EnvironmentInheritanceCase::Empty; "v2 excludes turn-only attachments")]
#[test_case(false, None, EnvironmentInheritanceCase::Fork; "v1 fork inherits configured attachments")]
#[test_case(true, None, EnvironmentInheritanceCase::Fork; "v2 fork inherits configured attachments")]
#[test_case(false, None, EnvironmentInheritanceCase::Pending; "v1 retains replaced pending attachment")]
#[test_case(true, None, EnvironmentInheritanceCase::Pending; "v2 retains replaced pending attachment")]
#[test_case(false, None, EnvironmentInheritanceCase::Legacy; "v1 retains turn attachments before rollout")]
#[test_case(true, None, EnvironmentInheritanceCase::Legacy; "v2 retains turn attachments before rollout")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_inherits_captured_settings_after_a_turn_update(
    multi_agent_v2: bool,
    requested_effort: Option<ReasoningEffort>,
    inheritance: EnvironmentInheritanceCase,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut spawn_arguments = json!({ "message": "Complete the delegated task." });
    let namespace = if multi_agent_v2 {
        spawn_arguments["task_name"] = json!("worker");
        spawn_arguments["fork_turns"] =
            json!(if matches!(inheritance, EnvironmentInheritanceCase::Fork) {
                "all"
            } else {
                "none"
            });
        "collaboration"
    } else {
        spawn_arguments["fork_context"] =
            json!(matches!(inheritance, EnvironmentInheritanceCase::Fork));
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
            if !matches!(inheritance, EnvironmentInheritanceCase::Legacy) {
                config
                    .features
                    .enable(Feature::SubagentThreadEnvironments)
                    .expect("enable configured environment inheritance");
            }
            config
                .features
                .enable(Feature::DeferredExecutor)
                .expect("enable deferred executor");
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collab");
            if multi_agent_v2 {
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("enable V2");
                config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::Encrypted;
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
    if matches!(inheritance, EnvironmentInheritanceCase::Empty) {
        submit_thread_settings(
            &test.codex,
            ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    Vec::new(),
                )),
                ..Default::default()
            },
        )
        .await?;
    }
    let mut configured = test.codex.environment_selections().await;
    if matches!(inheritance, EnvironmentInheritanceCase::Pending) {
        configured[0].config = EnvironmentConfigState::Pending;
        submit_thread_settings(
            &test.codex,
            ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    configured
                        .clone()
                        .into_iter()
                        .map(TurnEnvironmentSelection::into_request)
                        .collect(),
                )),
                ..Default::default()
            },
        )
        .await?;
    }
    let mut temporary = test.executor_environment().selection().clone();
    temporary.cwd = test.workspace_path_uri("turn-only-workspace")?;
    temporary.workspace_roots = vec![temporary.cwd.clone()];
    temporary.config =
        EnvironmentConfigState::Ready(environment_config_for_selection(&test.config, &temporary));
    test.fs()
        .create_directory(
            &temporary.cwd,
            CreateDirectoryOptions {
                recursive: false,
                follow_symlinks: true,
            },
            /*sandbox*/ None,
        )
        .await?;
    if matches!(inheritance, EnvironmentInheritanceCase::Legacy) {
        configured = vec![temporary.clone()];
    }
    let request = start_paused_turn(&test.codex).await?;
    apply_turn_settings(
        &test.codex,
        &request.turn_id,
        TurnSettingsUpdate {
            model: Some(MODEL_B.to_string()),
            effort: Some(Some(ReasoningEffort::High)),
            summary: Some(ReasoningSummary::Detailed),
            environments: Some(vec![temporary.into_request()]),
            ..Default::default()
        },
    )
    .await?;
    if matches!(inheritance, EnvironmentInheritanceCase::Pending) {
        let config = environment_config_for_selection(&test.config, &configured[0]);
        test.codex
            .environment_ready(&configured[0], config.clone())
            .await?;
        configured[0].config = EnvironmentConfigState::Ready(config);
    }
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
    assert_eq!(child_thread.environment_selections().await, configured);

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
