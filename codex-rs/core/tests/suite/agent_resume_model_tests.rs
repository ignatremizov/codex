//! Resume overrides use the target's settings and survive another close/reopen cycle.

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_extension_items::ExtensionItem;
use codex_features::Feature;
use codex_features::SleepToolMode;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::MockServer;

const ORIGINAL_MODEL: &str = "gpt-5.2";
const REQUESTED_MODEL: &str = "gpt-5.1";

fn handling() -> UserAgentResponseHandling {
    UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*queue_input*/ false,
    )
}

fn configure_model(model: &mut ModelInfo) {
    model.tool_mode = Some(ToolMode::Direct);
    model.default_reasoning_level = Some(ReasoningEffort::Low);
    model.supported_reasoning_levels = [ReasoningEffort::Low, ReasoningEffort::High]
        .into_iter()
        .map(|effort| ReasoningEffortPreset {
            description: effort.to_string(),
            effort,
        })
        .collect();
}

async fn fixture(
    history_mode: ThreadHistoryMode,
    version: MultiAgentVersion,
) -> Result<(MockServer, TestCodex, ThreadId)> {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_model_info_override(REQUESTED_MODEL, configure_model)
        .with_model_info_override(ORIGINAL_MODEL, configure_model)
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable agents");
            config
                .features
                .enable(Feature::SleepTool)
                .expect("enable sleep gate");
            config.sleep_tool_mode = SleepToolMode::AlwaysOn;
            if version == MultiAgentVersion::V2 {
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
            config.model_reasoning_effort = Some(ReasoningEffort::Low);
        })
        .build_with_auto_env(&server)
        .await?;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            model: Some(ORIGINAL_MODEL.into()),
            reasoning_effort: Some(ReasoningEffort::Low),
            response_handling: handling(),
            ..Default::default()
        })
        .await?
        .target_thread_id;
    Ok((server, test, child))
}

fn finish(id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_assistant_message(&format!("message-{id}"), "Done."),
        responses::ev_completed(id),
    ])
}

async fn resume(
    test: &TestCodex,
    server: &MockServer,
    child: ThreadId,
    call_id: &str,
    overrides: Value,
) -> Result<ResponsesRequest> {
    let (namespace, mut arguments) =
        if test.codex.multi_agent_version() == Some(MultiAgentVersion::V2) {
            ("collaboration", json!({"target": child.to_string()}))
        } else {
            ("multi_agent_v1", json!({"id": child.to_string(), "w": "x"}))
        };
    arguments
        .as_object_mut()
        .context("resume arguments")?
        .extend(overrides.as_object().context("resume overrides")?.clone());
    let response_id = format!("request-{call_id}");
    let captured = responses::mount_sse_sequence(
        server,
        vec![
            responses::sse(vec![
                responses::ev_response_created(&response_id),
                responses::ev_function_call_with_namespace(
                    call_id,
                    namespace,
                    "resume_agent",
                    &arguments.to_string(),
                ),
                responses::ev_completed(&response_id),
            ]),
            finish(&format!("done-{call_id}")),
        ],
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Apply the requested resume settings.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 2);
    requests
        .into_iter()
        .find(|request| request.function_call_output_text(call_id).is_some())
        .context("parent continuation with resume output")
}

#[derive(Clone, Copy)]
enum TargetState {
    Loaded,
    Closed,
}

#[derive(Clone, Copy)]
enum Overrides {
    Model,
    Effort,
    Both,
}

#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V1, TargetState::Loaded, Overrides::Both; "v1_live_legacy")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V1, TargetState::Loaded, Overrides::Both; "v1_live_paginated")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V1, TargetState::Closed, Overrides::Both; "v1_closed_legacy")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V1, TargetState::Closed, Overrides::Both; "v1_closed_paginated")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V1, TargetState::Loaded, Overrides::Model; "v1_model_only_live")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V1, TargetState::Closed, Overrides::Model; "v1_model_only_closed")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V1, TargetState::Loaded, Overrides::Effort; "v1_effort_only_live")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V1, TargetState::Closed, Overrides::Effort; "v1_effort_only_closed")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V2, TargetState::Loaded, Overrides::Both; "v2_live_legacy")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V2, TargetState::Loaded, Overrides::Both; "v2_live_paginated")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V2, TargetState::Closed, Overrides::Both; "v2_closed_legacy")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V2, TargetState::Closed, Overrides::Both; "v2_closed_paginated")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V2, TargetState::Loaded, Overrides::Model; "v2_model_only_live")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V2, TargetState::Closed, Overrides::Model; "v2_model_only_closed")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V2, TargetState::Loaded, Overrides::Effort; "v2_effort_only_live")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V2, TargetState::Closed, Overrides::Effort; "v2_effort_only_closed")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_updates_only_requested_choices_and_persists_them(
    history_mode: ThreadHistoryMode,
    version: MultiAgentVersion,
    state: TargetState,
    overrides: Overrides,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, test, child_id) = fixture(history_mode, version).await?;
    if matches!(state, TargetState::Closed) {
        test.codex
            .close_agent(&child_id.to_string(), handling())
            .await?;
    }
    let (arguments, expected_model, expected_effort) = match overrides {
        Overrides::Both => (
            json!({"model": REQUESTED_MODEL, "reasoning_effort": "high"}),
            REQUESTED_MODEL,
            ReasoningEffort::High,
        ),
        Overrides::Model => (
            json!({"model": REQUESTED_MODEL}),
            REQUESTED_MODEL,
            ReasoningEffort::Low,
        ),
        Overrides::Effort => (
            json!({"reasoning_effort": "high"}),
            ORIGINAL_MODEL,
            ReasoningEffort::High,
        ),
    };
    let response = resume(&test, &server, child_id, "resume-overrides", arguments).await?;
    let output: Value = serde_json::from_str(
        &response
            .function_call_output_text("resume-overrides")
            .context("resume output")?,
    )?;
    assert_eq!(
        output["status"],
        if version == MultiAgentVersion::V2 {
            json!("pending_init")
        } else {
            json!("idle")
        },
    );
    let child = test.thread_manager.get_thread(child_id).await?;
    let captured = responses::mount_sse_once(&server, finish("child-with-overrides")).await;
    test.codex
        .prompt_live_agent(
            &child_id.to_string(),
            vec![UserInput::Text {
                text: "Continue with your requested model settings.".into(),
                text_elements: Vec::new(),
            }],
            handling(),
        )
        .await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let body = captured.single_request().body_json();
    assert_eq!(
        (&body["model"], &body["reasoning"]["effort"]),
        (&json!(expected_model), &json!(expected_effort)),
    );
    let parent = test.codex.config_snapshot().await;
    assert_eq!(
        (parent.model, parent.reasoning_effort),
        (ORIGINAL_MODEL.into(), Some(ReasoningEffort::Low)),
    );
    test.codex
        .close_agent(&child_id.to_string(), handling())
        .await?;
    resume(&test, &server, child_id, "resume-saved", json!({})).await?;
    let restored = test.thread_manager.get_thread(child_id).await?;
    let saved = restored.config_snapshot().await;
    assert_eq!(
        (saved.model, saved.reasoning_effort),
        (expected_model.into(), Some(expected_effort)),
    );
    Ok(())
}

#[test_case(MultiAgentVersion::V1, json!({"model": "not-in-catalog"}), "Unknown model"; "v1_unknown_model")]
#[test_case(MultiAgentVersion::V1, json!({"reasoning_effort": "medium"}), "not supported"; "v1_unsupported_effort")]
#[test_case(MultiAgentVersion::V2, json!({"model": "not-in-catalog"}), "Unknown model"; "v2_unknown_model")]
#[test_case(MultiAgentVersion::V2, json!({"reasoning_effort": "medium"}), "not supported"; "v2_unsupported_effort")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_resume_choices_leave_the_target_settings_intact(
    version: MultiAgentVersion,
    overrides: Value,
    expected_error: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, test, child_id) = fixture(ThreadHistoryMode::Legacy, version).await?;
    let response = resume(&test, &server, child_id, "resume-invalid", overrides).await?;
    assert!(
        response
            .function_call_output_text("resume-invalid")
            .context("rejected resume output")?
            .contains(expected_error),
    );
    let child = test.thread_manager.get_thread(child_id).await?;
    let settings = child.config_snapshot().await;
    assert_eq!(
        (settings.model, settings.reasoning_effort),
        (ORIGINAL_MODEL.into(), Some(ReasoningEffort::Low)),
    );
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V1; "v1_legacy")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V1; "v1_paginated")]
#[test_case(ThreadHistoryMode::Legacy, MultiAgentVersion::V2; "v2_legacy")]
#[test_case(ThreadHistoryMode::Paginated, MultiAgentVersion::V2; "v2_paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_overrides_do_not_interrupt_or_reconfigure_the_active_turn(
    history_mode: ThreadHistoryMode,
    version: MultiAgentVersion,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, test, child_id) = fixture(history_mode, version).await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("child-working"),
            responses::ev_function_call_with_namespace(
                "work-gate",
                "clock",
                "sleep",
                &json!({"duration_ms": 60000}).to_string(),
            ),
            responses::ev_completed("child-working"),
        ]),
    )
    .await;
    let TurnInputSubmission::Started { turn_id, .. } = child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Wait for more input.".into(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        anyhow::bail!("expected a fresh child turn");
    };
    wait_for_event(&child, |event| {
        matches!(event, EventMsg::ItemStarted(event)
            if matches!(&event.item,
                TurnItem::Extension(ExtensionItem::Sleep(item)) if item.id == "work-gate"))
    })
    .await;
    let result = resume(
        &test,
        &server,
        child_id,
        "resume-running",
        json!({"model": REQUESTED_MODEL, "reasoning_effort": "high"}),
    )
    .await?;
    let output: Value = serde_json::from_str(
        &result
            .function_call_output_text("resume-running")
            .context("running resume output")?,
    )?;
    assert_eq!(output["status"], "running");
    let captured = responses::mount_sse_once(&server, finish("original-child-turn")).await;
    child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Finish your current turn.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let completed = wait_for_event(&child, |event| {
        assert!(
            !matches!(event, EventMsg::TurnAborted(_) | EventMsg::Error(_)),
            "resume must preserve the child turn: {event:?}",
        );
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(completed) = completed else {
        anyhow::bail!("expected child completion");
    };
    assert_eq!(completed.turn_id, turn_id);
    let body = captured.single_request().body_json();
    assert_eq!(
        (&body["model"], &body["reasoning"]["effort"]),
        (&json!(ORIGINAL_MODEL), &json!("low")),
    );
    let settings = child.config_snapshot().await;
    assert_eq!(
        (settings.model, settings.reasoning_effort),
        (REQUESTED_MODEL.into(), Some(ReasoningEffort::High)),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_resume_rejects_the_current_root() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, test, _) = fixture(ThreadHistoryMode::Legacy, MultiAgentVersion::V2).await?;
    let result = resume(
        &test,
        &server,
        test.session_configured.session_id.into(),
        "resume-root",
        json!({"model": REQUESTED_MODEL}),
    )
    .await?;
    assert!(
        result
            .function_call_output_text("resume-root")
            .context("root resume error")?
            .contains("non-root agent"),
    );
    assert_eq!(test.codex.config_snapshot().await.model, ORIGINAL_MODEL);
    Ok(())
}
