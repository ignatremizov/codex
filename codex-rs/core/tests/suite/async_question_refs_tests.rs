//! Short references survive model-window replacement and canonical cold resume.

use anyhow::Result;
use codex_context_fragments::AnsweredQuestion;
use codex_context_fragments::ContextualUserFragment;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_are_compact_and_numbers_survive_compaction_and_resume(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let ask = |response_id: &str, call_id: &str, questions| {
        responses::sse(vec![
            responses::ev_response_created(response_id),
            responses::ev_function_call_with_namespace(
                call_id,
                "functions",
                "request_user_input_async",
                &json!({"questions": questions}).to_string(),
            ),
            responses::ev_completed(response_id),
        ])
    };
    let finish = |id: &str| {
        responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(&format!("message-{id}"), "Recorded."),
            responses::ev_completed(id),
        ])
    };
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            ask(
                "first",
                "ask-first",
                json!([{"title": "Environment?"}, {"title": "Deadline?"}]),
            ),
            finish("first-done"),
            finish("answer-done"),
            finish("compact-done"),
            ask(
                "resumed",
                "ask-resumed",
                json!([{"title": "Anything else?"}]),
            ),
            finish("resumed-done"),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .disable(Feature::RemoteCompaction)
                .expect("disable remote compaction");
        })
        .with_model_info_override("gpt-5.2", |model| {
            model.tool_mode = Some(ToolMode::CodeModeOnly);
            model
                .experimental_supported_tools
                .push("request_user_input_async".to_string());
        });
    let initial = builder.build_with_auto_env(&server).await?;
    let input = |text: String| {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text,
            text_elements: Vec::new(),
        }])
    };
    initial
        .codex
        .start_or_steer_turn(input("Ask for details.".into()))
        .await?;
    let first_id = wait_for_event_match(&initial.codex, |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(message) if message.questions.is_some() => {
                Some(message.id.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(first_id, "async-question:q1:ask-first");
    assert_eq!(
        requests.requests()[1].function_call_output_text("ask-first"),
        Some(r#"{"accepted":true,"question_refs":["q1","q2"]}"#.into()),
    );
    let question_id = json!(["request_user_input_async", first_id, 1]).to_string();
    let reply = AnsweredQuestion::new(&question_id, "Deadline?", "Tomorrow").render();
    initial.codex.start_or_steer_turn(input(reply)).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(
        requests.requests()[2]
            .message_input_texts("user")
            .iter()
            .any(|text| text == "q2: Tomorrow")
    );
    initial.codex.submit(Op::Compact).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(
        requests.requests()[3]
            .message_input_texts("user")
            .iter()
            .any(|text| text == "q2: Tomorrow")
    );

    // Mutators are consumed by each build; restore the tool catalog for the new runtime.
    builder = builder.with_model_info_override("gpt-5.2", |model| {
        model.tool_mode = Some(ToolMode::CodeModeOnly);
        model
            .experimental_supported_tools
            .push("request_user_input_async".to_string());
    });
    let resumed = builder.restart(&server, &initial).await?;
    resumed
        .codex
        .start_or_steer_turn(input("Ask one more.".into()))
        .await?;
    let resumed_id = wait_for_event_match(&resumed.codex, |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(message) if message.questions.is_some() => {
                Some(message.id.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(resumed_id, "async-question:q3:ask-resumed");
    assert_eq!(
        requests.requests()[5].function_call_output_text("ask-resumed"),
        Some(r#"{"accepted":true,"question_refs":["q3"]}"#.into()),
    );
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
