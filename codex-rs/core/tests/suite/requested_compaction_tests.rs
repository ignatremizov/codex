//! Explicit compaction waits for tools, preserves the active turn, and resumes real work.

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::strip_response_item_ids_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use test_case::test_case;

const USER_TASK: &str = "The task changed: use the screenshot findings to finish the new goal.";
const FINDING: &str =
    "Screenshot finding: the retry button is disabled until validation completes.";
const CHECKPOINT: &str = "Preserve the retry-button finding and finish the new user goal.";
const GATE_CALL: &str = "work-before-compact";
const MODEL: &str = "gpt-5.6-sol";

#[derive(Clone, Copy)]
enum Compaction {
    Local,
    Remote,
    TokenBudget,
}

#[derive(Clone, Copy)]
enum RequestSource {
    Model,
    User,
}

fn builder(compaction: Compaction) -> TestCodexBuilder {
    test_codex()
        .with_model(MODEL)
        .with_model_info_override(MODEL, |model| {
            model.comp_hash = None;
            model.tool_mode = Some(ToolMode::Direct);
        })
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config
                .features
                .disable(Feature::AsyncCompaction)
                .expect("disable speculative requests");
            match compaction {
                Compaction::Remote => config.features.enable(Feature::RemoteCompaction),
                Compaction::Local | Compaction::TokenBudget => {
                    config.features.disable(Feature::RemoteCompaction)
                }
            }
            .expect("select compaction implementation");
            match compaction {
                Compaction::TokenBudget => config.features.enable(Feature::TokenBudget),
                Compaction::Local | Compaction::Remote => {
                    config.features.disable(Feature::TokenBudget)
                }
            }
            .expect("select context policy");
            config.update_plan_enabled = true;
            config.model_post_turn_compact_threshold_percent = 0;
            config.remote_compaction_handoff_enabled = false;
            config.model_provider.stream_max_retries = Some(0);
        })
}

async fn gated_thread(
    test: &TestCodex,
    history_mode: ThreadHistoryMode,
) -> Result<Arc<CodexThread>> {
    Ok(test
        .thread_manager
        .start_thread(StartThreadOptions {
            history_mode: Some(history_mode),
            dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "work_gate".into(),
                description: "Complete outstanding analysis.".into(),
                input_schema: json!({"type": "object", "properties": {}}),
                defer_loading: false,
            })],
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread)
}

#[test_case(Compaction::Local, RequestSource::Model, ThreadHistoryMode::Legacy; "model_local_legacy")]
#[test_case(Compaction::Local, RequestSource::User, ThreadHistoryMode::Legacy; "user_local_legacy")]
#[test_case(Compaction::Remote, RequestSource::Model, ThreadHistoryMode::Legacy; "model_remote_legacy")]
#[test_case(Compaction::Remote, RequestSource::User, ThreadHistoryMode::Legacy; "user_remote_legacy")]
#[test_case(Compaction::Local, RequestSource::Model, ThreadHistoryMode::Paginated; "model_local_paginated")]
#[test_case(Compaction::Local, RequestSource::User, ThreadHistoryMode::Paginated; "user_local_paginated")]
#[test_case(Compaction::Remote, RequestSource::Model, ThreadHistoryMode::Paginated; "model_remote_paginated")]
#[test_case(Compaction::Remote, RequestSource::User, ThreadHistoryMode::Paginated; "user_remote_paginated")]
#[test_case(Compaction::TokenBudget, RequestSource::Model, ThreadHistoryMode::Legacy; "model_token_budget")]
#[test_case(Compaction::TokenBudget, RequestSource::User, ThreadHistoryMode::Paginated; "user_token_budget")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requested_compaction_waits_for_tools_and_continues_the_same_turn(
    compaction: Compaction,
    source: RequestSource,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = builder(compaction).build_with_auto_env(&server).await?;
    let thread = gated_thread(&test, history_mode).await?;

    let mut first = vec![responses::ev_response_created("analysis")];
    if matches!(source, RequestSource::Model) {
        first.push(responses::ev_function_call("self-compact", "compact", "{}"));
    }
    first.extend([
        responses::ev_function_call(GATE_CALL, "work_gate", "{}"),
        responses::ev_completed("analysis"),
    ]);
    let mut sequence = vec![responses::sse(first)];
    if !matches!(compaction, Compaction::TokenBudget) {
        let item = match compaction {
            Compaction::Local => responses::ev_assistant_message("checkpoint", CHECKPOINT),
            Compaction::Remote => json!({
                "type": "response.output_item.done",
                "item": {"type": "compaction", "encrypted_content": "EXPLICIT_CHECKPOINT"},
            }),
            Compaction::TokenBudget => unreachable!(),
        };
        sequence.push(responses::sse(vec![
            item,
            responses::ev_completed("compacted"),
        ]));
    }
    sequence.extend([
        responses::sse(vec![
            responses::ev_function_call(
                "continue-work",
                "update_plan",
                &json!({"plan": [{"step": "Finish the new goal", "status": "in_progress"}]})
                    .to_string(),
            ),
            responses::ev_completed("continued"),
        ]),
        responses::sse(vec![
            responses::ev_assistant_message("finished-message", "Finished the new goal."),
            responses::ev_completed("finished"),
        ]),
    ]);
    let captured = responses::mount_sse_sequence(&server, sequence).await;
    let submitted = thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: USER_TASK.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id, .. } = submitted else {
        anyhow::bail!("expected a fresh turn: {submitted:?}");
    };
    wait_for_event(
        &thread,
        |event| matches!(event, EventMsg::DynamicToolCallRequest(request) if request.call_id == GATE_CALL),
    )
    .await;
    // Admission is ordered before the gate reply. Repeated requests must cost one compaction.
    if matches!(source, RequestSource::User) {
        thread.submit(Op::Compact).await?;
        thread.submit(Op::Compact).await?;
    }
    assert_eq!(captured.requests().len(), 1);
    thread
        .submit(Op::DynamicToolResponse {
            id: GATE_CALL.into(),
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: FINDING.into(),
                }],
                success: true,
            },
        })
        .await?;
    let mut compacted_turns = Vec::new();
    let complete = wait_for_event(&thread, |event| {
        match event {
            EventMsg::ItemCompleted(event)
                if matches!(event.item, TurnItem::ContextCompaction(_)) =>
            {
                compacted_turns.push(event.turn_id.clone());
            }
            EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) | EventMsg::Error(_) => {
                panic!("compaction must stay in its regular turn: {event:?}");
            }
            _ => {}
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(complete) = complete else {
        anyhow::bail!("expected turn completion");
    };
    assert_eq!(
        (compacted_turns, complete.turn_id, complete.error),
        (vec![turn_id.clone()], turn_id, None)
    );
    let requests = captured.requests();
    let input = requests[0].input();
    let declaration = input
        .iter()
        .rfind(|item| item["type"] == "additional_tools")
        .context("request declares additional tools")?;
    let compact = responses::namespace_child_tool(declaration, "functions", "compact")
        .context("request exposes compact")?;
    let description = compact["description"]
        .as_str()
        .context("compact has a description")?;
    assert_eq!(
        description.contains("without summarizing"),
        matches!(compaction, Compaction::TokenBudget),
        "compact instructions must match the active context policy"
    );
    let next = match compaction {
        Compaction::Local | Compaction::Remote => {
            assert_eq!(requests.len(), 4);
            let compaction_input = requests[1].input();
            assert!(compaction_input.iter().any(|item| {
                item["role"] == "user"
                    && item["content"]
                        .as_array()
                        .is_some_and(|content| content.iter().any(|part| part["text"] == USER_TASK))
            }));
            assert_eq!(
                requests[1]
                    .function_call_output_content_and_success(GATE_CALL)
                    .and_then(|(text, _)| text),
                Some(FINDING.into())
            );
            if matches!(source, RequestSource::Model) {
                let compact_result = requests[1].function_call_output("self-compact");
                assert!(
                    compact_result
                        .to_string()
                        .contains("continue automatically")
                );
            }
            &requests[2]
        }
        Compaction::TokenBudget => {
            assert_eq!(requests.len(), 3);
            &requests[1]
        }
    };
    match compaction {
        Compaction::Local => {
            assert!(next.input().iter().any(|item| {
                item["role"] == "user"
                    && item["content"].as_array().is_some_and(|content| {
                        content.iter().any(|part| {
                            part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains(CHECKPOINT))
                        })
                    })
            }));
        }
        Compaction::Remote => assert_eq!(
            strip_response_item_ids_from_json(json!(next.inputs_of_type("compaction"))),
            json!([{"type": "compaction", "encrypted_content": "EXPLICIT_CHECKPOINT"}])
        ),
        Compaction::TokenBudget => {}
    }
    let final_request = requests.last().context("final sampling request")?;
    assert!(
        !final_request
            .function_call_output("continue-work")
            .is_null()
    );
    Ok(())
}

#[test_case(RequestSource::Model; "model")]
#[test_case(RequestSource::User; "user")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_request_does_not_compact_a_later_turn(source: RequestSource) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = builder(Compaction::Local)
        .build_with_auto_env(&server)
        .await?;
    let thread = gated_thread(&test, ThreadHistoryMode::Legacy).await?;
    let mut first = Vec::new();
    if matches!(source, RequestSource::Model) {
        first.push(responses::ev_function_call("self-compact", "compact", "{}"));
    }
    first.extend([
        responses::ev_function_call(GATE_CALL, "work_gate", "{}"),
        responses::ev_completed("interrupted-analysis"),
    ]);
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(first),
            responses::sse(vec![
                responses::ev_assistant_message("next-task", "Finished the next task."),
                responses::ev_completed("next-task"),
            ]),
        ],
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: USER_TASK.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(
        &thread,
        |event| matches!(event, EventMsg::DynamicToolCallRequest(request) if request.call_id == GATE_CALL),
    )
    .await;
    if matches!(source, RequestSource::User) {
        thread.submit(Op::Compact).await?;
    }
    thread.submit(Op::Interrupt).await?;
    wait_for_event(&thread, |event| {
        assert!(
            !matches!(event, EventMsg::ItemStarted(started) if matches!(started.item, TurnItem::ContextCompaction(_)))
        );
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let next_turn = thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Start a separate task after the interrupted analysis.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    assert!(matches!(next_turn, TurnInputSubmission::Started { .. }));
    wait_for_event(&thread, |event| {
        assert!(
            !matches!(event, EventMsg::ItemStarted(started) if matches!(started.item, TurnItem::ContextCompaction(_)))
        );
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(captured.requests().len(), 2);
    Ok(())
}

#[test_case(Compaction::Local; "local")]
#[test_case(Compaction::Remote; "remote")]
#[tokio::test]
async fn failed_explicit_compaction_reports_failure_without_retrying_the_task(
    compaction: Compaction,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let captured = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call("self-compact", "compact", "{}"),
                responses::ev_completed("compact-request"),
            ])),
            wiremock::ResponseTemplate::new(/*status*/ 400).set_body_json(json!({
                "error": {"type": "invalid_request_error", "message": "Compaction was rejected"},
            })),
        ],
    )
    .await;
    let test = builder(compaction).build_with_auto_env(&server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: USER_TASK.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut errors = Vec::new();
    let completed = wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            errors.push(error.message.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(completed) = completed else {
        anyhow::bail!("expected completion after compaction failure");
    };
    assert!(completed.error.is_some());
    assert!(
        errors
            .iter()
            .any(|error| error.contains("Compaction was rejected")),
        "expected the compaction failure, got {errors:?}",
    );
    assert_eq!(captured.requests().len(), 2);
    Ok(())
}

#[tokio::test]
async fn invalid_self_compaction_arguments_return_feedback_without_compacting() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_function_call("bad-compact", "compact", r#"{"thread_id":"another"}"#),
                responses::ev_completed("invalid-request"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("recovered", "Continuing without compaction."),
                responses::ev_completed("recovered"),
            ]),
        ],
    )
    .await;
    let test = builder(Compaction::Local)
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: USER_TASK.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        assert!(
            !matches!(event, EventMsg::ItemStarted(started) if matches!(started.item, TurnItem::ContextCompaction(_)))
        );
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .function_call_output("bad-compact")
            .to_string()
            .contains("unknown field")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_child_compacts_itself_without_compacting_its_parent() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_function_call("child-compact", "compact", "{}"),
                responses::ev_completed("child-analysis"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("child-checkpoint", CHECKPOINT),
                responses::ev_completed("child-compacted"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("child-finished", "Finished the child task."),
                responses::ev_completed("child-finished"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("parent-finished", "Finished the parent task."),
                responses::ev_completed("parent-finished"),
            ]),
        ],
    )
    .await;
    let test = builder(Compaction::Local)
        .with_config(|config| {
            config.features.enable(Feature::Collab).unwrap();
            config.features.disable(Feature::MultiAgentV2).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    let handling = UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*queue_input*/ false,
    );
    let child_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: handling,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let child = test.thread_manager.get_thread(child_id).await?;
    test.codex
        .prompt_live_agent(
            &child_id.to_string(),
            vec![UserInput::Text {
                text: USER_TASK.into(),
                text_elements: Vec::new(),
            }],
            handling,
        )
        .await?;
    let mut compaction_threads = Vec::new();
    wait_for_event(&child, |event| {
        if let EventMsg::ItemCompleted(event) = event
            && matches!(event.item, TurnItem::ContextCompaction(_))
        {
            compaction_threads.push(event.thread_id);
        }
        assert!(!matches!(event, EventMsg::Error(_)));
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(compaction_threads, vec![child_id]);
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue the parent's separate task.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        assert!(
            !matches!(event, EventMsg::ItemStarted(started) if matches!(started.item, TurnItem::ContextCompaction(_)))
        );
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 4);
    assert!(!requests[3].body_contains_text(CHECKPOINT));
    Ok(())
}
