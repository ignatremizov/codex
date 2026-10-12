//! Future-peer discovery reuses hydrated identity without sharing the peer's assigned work.

use anyhow::Result;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
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
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sibling_discovery_keeps_concurrent_prompts_isolated(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    // Unrelated opaque payloads cannot be guessed from a sibling's contact identity.
    const FIRST_PROMPT: &str = "Report the alpha marker: cobalt-7f2c.";
    const SIBLING_PROMPT: &str = "Report the beta marker: orchid-95e1.";
    const CALL_ID: &str = "first-agent-plan";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (release, gate) = oneshot::channel();
    let mut first = server
        .mount_response(
            |request| {
                request.body_contains_text(FIRST_PROMPT) && !request.body_contains_text(CALL_ID)
            },
            vec![
                StreamingSseChunk {
                    gate: None,
                    body: sse(vec![ev_response_created("first")]),
                },
                StreamingSseChunk {
                    gate: Some(gate),
                    body: sse(vec![
                        ev_function_call(
                            CALL_ID,
                            "update_plan",
                            r#"{"plan":[{"step":"Report assigned marker","status":"completed"}]}"#,
                        ),
                        ev_completed("first"),
                    ]),
                },
            ],
        )
        .await;
    let mut sibling = server
        .mount_response(
            |request| request.body_contains_text(SIBLING_PROMPT),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("sibling"),
                    ev_assistant_message("sibling-done", "orchid-95e1"),
                    ev_completed("sibling"),
                ]),
            }],
        )
        .await;
    let mut followup = server
        .mount_response(
            |request| request.body_contains_text(CALL_ID),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("first-followup"),
                    ev_assistant_message("first-done", "cobalt-7f2c"),
                    ev_completed("first-followup"),
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
    let mut children = Vec::new();
    let mut first_identity = Vec::new();
    for prompt in [FIRST_PROMPT, SIBLING_PROMPT] {
        let spawned = test
            .codex
            .spawn_agent(UserAgentSpawnOptions {
                input: Some(vec![UserInput::Text {
                    text: prompt.into(),
                    text_elements: Vec::new(),
                }]),
                response_handling: UserAgentResponseHandling::Presentation,
                ..Default::default()
            })
            .await?;
        let child_id = spawned.target_thread_id;
        children.push((
            child_id,
            test.thread_manager.get_thread(child_id).await?,
            spawned.agent_ref,
        ));
        if prompt == FIRST_PROMPT {
            let request =
                tokio::time::timeout(Duration::from_secs(/*secs*/ 10), first.wait_for_request())
                    .await?;
            assert!(!request.body_contains_text(SIBLING_PROMPT));
            first_identity = request
                .message_input_texts("developer")
                .into_iter()
                .filter(|text| text.starts_with("<agent_identity_context>"))
                .collect::<Vec<_>>();
        }
    }
    let sibling_request =
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), sibling.wait_for_request()).await?;
    assert!(!sibling_request.body_contains_text(FIRST_PROMPT));
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("first response gate closed"))?;
    let request = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        followup.wait_for_request(),
    )
    .await?;
    assert!(!request.body_contains_text(SIBLING_PROMPT));
    let peer = children[1]
        .2
        .ok_or_else(|| anyhow::anyhow!("missing sibling ref"))?
        .to_string();
    let identities = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with("<agent_identity_context>"))
        .collect::<Vec<_>>();
    assert_eq!(first_identity.len(), 1);
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0], first_identity[0]);
    let delta = identities[1]
        .strip_prefix("<agent_identity_context>\n")
        .and_then(|text| text.strip_suffix("\n</agent_identity_context>"))
        .ok_or_else(|| anyhow::anyhow!("missing identity delta"))?;
    let delta: Value = serde_json::from_str(delta)?;
    let added = delta["upsert"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing identity upserts"))?;
    assert_eq!(added.len(), 1);
    assert_eq!(added[0]["ref"], json!(peer));
    assert_eq!(delta, json!({"upsert": added}));
    for (_, child, _) in children {
        wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }
    assert_eq!(server.requests().await.len(), 3);
    server.shutdown().await;
    Ok(())
}
