//! Spawned agents can ask the user directly without routing questions or answers through Main.

use anyhow::Context;
use anyhow::Result;
use codex_context_fragments::AnsweredQuestion;
use codex_context_fragments::ContextualUserFragment;
use codex_core::TurnInputRequest;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_protocol::items::AgentMessageDelivery;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_protocol_test_support::ContentItemTestExt;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_agent_asks_and_receives_its_own_user_answer(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const CALL: &str = "child-asks-user";
    const QUESTION: &str = "Which deployment should this agent inspect?";
    const ANSWER: &str = "Inspect the private staging deployment";
    let server = responses::start_mock_server().await;
    let finish = |id: &str| {
        responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(&format!("message-{id}"), "Recorded."),
            responses::ev_completed(id),
        ])
    };
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("child-question"),
                responses::ev_function_call_with_namespace(
                    CALL,
                    "functions",
                    "request_user_input_async",
                    &json!({"questions": [{"title": QUESTION}]}).to_string(),
                ),
                responses::ev_completed("child-question"),
            ]),
            finish("child-question-done"),
            finish("child-answer-done"),
            finish("main-done"),
        ],
    )
    .await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable native agent tools");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("select V1 agent tools");
            config.experimental_request_user_input_enabled = true;
        })
        .with_model_info_override("gpt-5.2", |model| {
            model.tool_mode = Some(ToolMode::CodeModeOnly);
            model
                .experimental_supported_tools
                .push("request_user_input_async".into());
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
                text: "Ask the user which deployment to inspect.".into(),
                text_elements: Vec::new(),
            }],
            handling,
        )
        .await?;
    let (requester, message) = wait_for_event_match(&child, |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(message)
                if message.delivery == Some(AgentMessageDelivery::Async) =>
            {
                Some((event.thread_id, message.clone()))
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(requester, child_id);
    assert_eq!(
        message.questions,
        Some(vec![codex_protocol::items::AsyncUserInputQuestion {
            title: QUESTION.into(),
            options: None,
        }])
    );
    let question_id = json!(["request_user_input_async", message.id, 0]).to_string();
    let reply = AnsweredQuestion::new(&question_id, QUESTION, ANSWER)
        .render()
        .expect_text();
    test.codex
        .prompt_live_agent(
            &requester.to_string(),
            vec![UserInput::Text {
                text: reply,
                text_elements: Vec::new(),
            }],
            handling,
        )
        .await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue the main task.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 4);
    let tool = requests[0].body_json()["tools"]
        .as_array()
        .and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool["name"] == "request_user_input_async")
                .cloned()
        })
        .ok_or_else(|| anyhow::anyhow!("child question tool"))?;
    assert!(
        tool["description"]
            .as_str()
            .context("child question tool description")?
            .contains("requesting agent")
    );
    assert_eq!(
        requests[1].function_call_output_text(CALL),
        Some(r#"{"accepted":true,"question_refs":["q1"]}"#.into())
    );
    assert_eq!(
        requests[2]
            .message_input_texts("user")
            .into_iter()
            .filter(|text| text.contains(ANSWER))
            .collect::<Vec<_>>(),
        vec![format!("q1: {ANSWER}")]
    );
    assert!(
        !requests[3].input().iter().any(|item| {
            let text = item.to_string();
            text.contains(ANSWER) || text.contains(QUESTION)
        }),
        "Main must not receive the child's question or answer in model context"
    );
    Ok(())
}
