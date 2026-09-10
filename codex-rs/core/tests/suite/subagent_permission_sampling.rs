use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::UserAgentReplyRouteMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;

#[derive(Clone, Copy)]
enum SamplingBoundary {
    FinalAnswer,
    ToolFollowup,
}

#[test_case(ThreadHistoryMode::Legacy, SamplingBoundary::FinalAnswer; "legacy final")]
#[test_case(ThreadHistoryMode::Paginated, SamplingBoundary::FinalAnswer; "paginated final")]
#[test_case(ThreadHistoryMode::Legacy, SamplingBoundary::ToolFollowup; "legacy tool")]
#[test_case(ThreadHistoryMode::Paginated, SamplingBoundary::ToolFollowup; "paginated tool")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_context_is_durable_without_requesting_a_response(
    history_mode: ThreadHistoryMode,
    boundary: SamplingBoundary,
) -> Result<()> {
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let mut final_answer = ev_assistant_message("first-final", "Standing by.");
    final_answer["item"]["phase"] = serde_json::json!("final_answer");
    let (before_update, after_update) = match boundary {
        SamplingBoundary::FinalAnswer => (
            vec![ev_response_created("first"), final_answer],
            vec![ev_completed("first")],
        ),
        SamplingBoundary::ToolFollowup => (
            vec![ev_response_created("first")],
            vec![
                ev_function_call(
                    "plan-call",
                    "update_plan",
                    r#"{"plan":[{"step":"Inspect permissions","status":"completed"}]}"#,
                ),
                ev_completed("first"),
            ],
        ),
    };
    let mut first = server
        .mount_response(
            |_| true,
            vec![
                StreamingSseChunk {
                    gate: None,
                    body: sse(before_update),
                },
                StreamingSseChunk {
                    gate: Some(gate),
                    body: sse(after_update),
                },
            ],
        )
        .await;
    // Also answers an erroneous permission-only followup so that the regression
    // fails on the request count, rather than hanging on an unmatched request.
    let mut next_answer = ev_assistant_message("second-final", "Permissions reviewed.");
    next_answer["item"]["phase"] = serde_json::json!("final_answer");
    let mut second = server
        .mount_response(
            |_| true,
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("second"),
                    next_answer,
                    ev_completed("second"),
                ]),
            }],
        )
        .await;
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        });
    let test = builder
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let child_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let child = test.thread_manager.get_thread(child_id).await?;
    child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Stand by.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let first_request =
        tokio::time::timeout(Duration::from_secs(10), first.wait_for_request()).await?;
    if let SamplingBoundary::FinalAnswer = boundary {
        wait_for_event(&child, |event| {
            matches!(event, EventMsg::ItemCompleted(event)
                if matches!(&event.item, TurnItem::AgentMessage(message)
                    if message.phase == Some(MessagePhase::FinalAnswer)))
        })
        .await;
    }

    test.codex
        .set_agent_subtree_messaging(UserAgentReplyRouteMode::Enabled)
        .await?;
    // The stream is still gated: persistence must not depend on another sample
    // or even completion of the current response.
    child.flush_rollout().await?;
    let history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: child_id,
            include_archived: false,
        })
        .await?;
    let notices = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::Message { role, content, .. } if role == "developer" => {
                    match content.as_slice() {
                        [ContentItem::InputText { text }]
                            if text.starts_with("User enabled send_input within ") =>
                        {
                            Some(text.clone())
                        }
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    let notice = &notices[0];
    assert!(!first_request.body_contains_text(notice));
    release.send(()).expect("release response completion");
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    if let SamplingBoundary::FinalAnswer = boundary {
        assert_eq!(server.requests().await.len(), 1);
        child
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Review the current permissions.".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }
    let next_request =
        tokio::time::timeout(Duration::from_secs(10), second.wait_for_request()).await?;
    assert!(next_request.body_contains_text(notice));
    assert_eq!(server.requests().await.len(), 2);
    if let SamplingBoundary::ToolFollowup = boundary {
        let body = next_request.body_json();
        assert!(
            body["input"]
                .as_array()
                .expect("input array")
                .iter()
                .any(
                    |item| item["type"] == "function_call_output" && item["call_id"] == "plan-call"
                )
        );
    }
    server.shutdown().await;
    Ok(())
}
