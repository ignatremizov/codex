//! A child outside the wait targets can steer its waiting parent and continue that turn.

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_send_input_releases_parent_waiting_on_another_child(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const WAIT: &str = "parent-waits";
    const SEND: &str = "child-steers-parent";
    const MESSAGE: &str = "The requested investigation is ready; continue now.";
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_model("gpt-5.2")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable native agent tools");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("select V1 agent tools");
        })
        .with_model_info_override("gpt-5.2", |model| {
            model.tool_mode = Some(ToolMode::Direct);
        })
        .build_with_auto_env(&server)
        .await?;
    let handling = UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*queue_input*/ false,
    );
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            test.codex
                .spawn_agent(UserAgentSpawnOptions {
                    response_handling: handling,
                    ..Default::default()
                })
                .await?
                .target_thread_id,
        );
    }
    let target = test.thread_manager.get_thread(children[0]).await?;
    let sender = test.thread_manager.get_thread(children[1]).await?;
    let finish = || {
        responses::sse(vec![
            responses::ev_response_created("continued"),
            responses::ev_assistant_message("continued-message", "Continued."),
            responses::ev_completed("continued"),
        ])
    };
    let captured = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("wait"),
                responses::ev_function_call_with_namespace(
                    WAIT,
                    "multi_agent_v1",
                    "wait_agent",
                    &json!({"targets": [children[0].to_string()], "timeout_ms": 60000}).to_string(),
                ),
                responses::ev_completed("wait"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("send"),
                responses::ev_function_call_with_namespace(
                    SEND,
                    "multi_agent_v1",
                    "send_input",
                    &json!({"target": "Main", "message": MESSAGE}).to_string(),
                ),
                responses::ev_completed("send"),
            ]),
            finish(),
            finish(),
        ],
    )
    .await;
    let TurnInputSubmission::Started {
        turn_id: parent_turn,
        ..
    } = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Wait for the first child.".into(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        anyhow::bail!("expected a new parent turn");
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ItemStarted(started)
            if matches!(&started.item, TurnItem::CollabAgentToolCall(item)
                if item.tool == CollabAgentTool::Wait))
    })
    .await;
    test.codex
        .prompt_live_agent(
            &children[1].to_string(),
            vec![UserInput::Text {
                text: "Send your findings to Main.".into(),
                text_elements: Vec::new(),
            }],
            handling,
        )
        .await?;
    let completed = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_for_event(&sender, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert!(
        matches!(completed, EventMsg::TurnComplete(event) if event.turn_id == parent_turn),
        "the steered parent must continue in the same turn",
    );
    assert_eq!(target.agent_status().await, AgentStatus::PendingInit);
    let requests = captured.requests();
    assert_eq!(requests.len(), 4);
    let wait_output = requests
        .iter()
        .find_map(|request| {
            request
                .function_call_output_text(WAIT)
                .map(|output| (request, output))
        })
        .context("parent continuation with wait output")?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&wait_output.1)?,
        json!({"status": {}, "timed_out": false, "return_reason": "input"}),
    );
    assert_eq!(
        wait_output
            .0
            .message_input_texts("user")
            .iter()
            .map(|text| text.matches(MESSAGE).count())
            .sum::<usize>(),
        1,
    );
    Ok(())
}
