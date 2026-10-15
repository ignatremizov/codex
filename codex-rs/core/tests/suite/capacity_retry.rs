use anyhow::Result;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use wiremock::ResponseTemplate;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_retry_keeps_input_and_drained_tool_results_once(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let first = responses::sse(vec![
        responses::ev_response_created("partial"),
        responses::ev_function_call(
            "plan-before-capacity",
            "update_plan",
            &json!({"plan": [{"step": "one observable effect", "status": "in_progress"}]})
                .to_string(),
        ),
        json!({
            "type": "response.failed",
            "response": {"error": {"code": "server_is_overloaded", "message": "overloaded"}}
        }),
    ]);
    let requests = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(first),
            ResponseTemplate::new(/*status*/ 503)
                .insert_header("Retry-After", "0")
                .set_body_json(json!({"error": {"code": "server_is_overloaded"}})),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_assistant_message("answer", "Plan retained."),
                responses::ev_completed("recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.update_plan_enabled = true;
            // Capacity has an independent budget and never falls back to a different transport.
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.request_max_retries = Some(3);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    let prompt = "Update the plan once, then report the result.";
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut starts = 0;
    let mut inputs = 0;
    let mut effects = 0;
    let mut retries = 0;
    let mut turn_id = None;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::TurnStarted(_) => {
                    starts += 1;
                    turn_id = Some(event.id);
                }
                EventMsg::UserMessage(_) => inputs += 1,
                EventMsg::PlanUpdate(_) => effects += 1,
                EventMsg::StreamError(error) => {
                    retries += 1;
                    assert_eq!(Some(event.id), turn_id);
                    assert_eq!(
                        error.codex_error_info,
                        Some(CodexErrorInfo::ServerOverloaded)
                    );
                    assert!(error.retry_at.is_some());
                }
                EventMsg::Error(error) => anyhow::bail!("unexpected terminal error: {error:?}"),
                EventMsg::TurnComplete(complete) => {
                    assert_eq!(Some(event.id), turn_id);
                    assert!(complete.error.is_none());
                    break;
                }
                _ => {}
            }
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!((starts, inputs, effects, retries), (1, 1, 1, 2));
    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(
            request.body_json()["model"],
            requests[0].body_json()["model"]
        );
        assert_eq!(
            request
                .message_input_texts("user")
                .iter()
                .filter(|text| text.as_str() == prompt)
                .count(),
            1
        );
        assert_eq!(
            request
                .inputs_of_type("function_call_output")
                .iter()
                .filter(|item| item["call_id"] == "plan-before-capacity")
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_capacity_wait_without_another_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let requests = responses::mount_response_once(
        &server,
        ResponseTemplate::new(/*status*/ 503)
            .insert_header("Retry-After", "60")
            .set_body_json(json!({"error": {"code": "server_is_overloaded"}})),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.model_provider.request_max_retries = Some(3))
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Wait until interrupted.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::StreamError(_))
    })
    .await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert_eq!(requests.requests().len(), 1);
    let next = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("next-turn"),
            responses::ev_assistant_message("next-answer", "Done."),
            responses::ev_completed("next-turn"),
        ]),
    )
    .await;
    test.submit_turn("Answer this new turn normally.").await?;
    assert_eq!(next.requests().len(), 1);
    Ok(())
}
