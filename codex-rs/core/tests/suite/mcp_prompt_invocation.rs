use anyhow::Context;
use anyhow::Result;
use codex_config::McpServerConfig;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use wiremock::MockServer;

use super::rmcp_client::remote_aware_environment_id;
use super::rmcp_client::remote_aware_stdio_server_bin;
use super::rmcp_client::remote_aware_stdio_server_cwd;

const SERVER: &str = "explicit_docs";
const NAMESPACE: &str = "mcp__explicit_docs";

async fn fixture(server: &MockServer, implicit: bool) -> Result<TestCodex> {
    let mcp: McpServerConfig = serde_json::from_value(json!({
        "command": remote_aware_stdio_server_bin()?,
        "environment_id": remote_aware_environment_id(),
        "cwd": remote_aware_stdio_server_cwd(),
        "allow_implicit_invocation": implicit,
        "enabled_tools": ["echo"],
        "startup_timeout_sec": 10
    }))?;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(SERVER.to_string(), mcp);
            config.mcp_servers.set(servers).expect("MCP configuration");
        })
        .build_with_auto_env(server)
        .await?;
    wait_for_mcp_server(&test.codex, SERVER).await?;
    Ok(test)
}

async fn completion(server: &MockServer, id: &str) -> ResponseMock {
    responses::mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(id, "done"),
            responses::ev_completed(id),
        ]),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_hidden_inventory_is_forward_only_callable_and_does_not_start_idle_inference()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(Ok(()), "requires the executor MCP test binary");
    let server = responses::start_mock_server().await;
    let test = fixture(&server, /*implicit*/ false).await?;
    let first = completion(&server, "first").await;
    test.submit_turn("first turn without explicit use").await?;
    assert_eq!(first.single_request().tool_by_name(NAMESPACE, "echo"), None);
    assert!(
        !first
            .single_request()
            .message_input_texts("developer")
            .iter()
            .any(|text| text.contains("<mcp_use>"))
    );

    test.codex
        .submit(Op::ActivateMcpServer {
            server_name: SERVER.to_string(),
        })
        .await?;
    let text = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(text) = test.codex.latest_mcp_server_use_context_text(SERVER).await {
                break text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("activation should append context while idle")?;
    assert!(text.contains("\"inputSchema\"") && text.contains("\"echo\""));
    assert_eq!(first.requests().len(), 1);
    test.codex
        .submit(Op::ActivateMcpServer {
            server_name: SERVER.to_string(),
        })
        .await?;

    let call = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("call"),
            responses::ev_function_call_with_namespace(
                "echo-call",
                NAMESPACE,
                "echo",
                r#"{"message":"explicit"}"#,
            ),
            responses::ev_completed("call"),
        ]),
    )
    .await;
    let done = completion(&server, "done").await;
    test.codex
        .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
            codex_protocol::user_input::UserInput::Text {
                text: "use the explicitly selected echo tool".to_string(),
                text_elements: Vec::new(),
            },
        ]))
        .await?;
    let EventMsg::McpToolCallEnd(result) = wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::McpToolCallEnd(end) if end.call_id == "echo-call"),
    )
    .await
    else {
        unreachable!("matching MCP call event");
    };
    assert!(
        result.result.is_ok(),
        "explicit selection retains runtime callability"
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = call.single_request();
    assert_eq!(request.tool_by_name(NAMESPACE, "echo"), None);
    assert_eq!(
        request
            .message_input_texts("developer")
            .into_iter()
            .filter(|text| text.contains("<mcp_use>"))
            .collect::<Vec<_>>(),
        vec![text],
    );
    assert_eq!(done.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_first_turn_activation_does_not_freeze_or_duplicate_the_direct_contract()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(Ok(()), "requires the executor MCP test binary");
    for implicit in [false, true] {
        let server = responses::start_mock_server().await;
        let test = fixture(&server, implicit).await?;
        assert!(
            !test
                .codex
                .mcp_server_would_be_direct_at_session_start(SERVER)
                .await
        );
        test.codex
            .submit(Op::ActivateMcpServer {
                server_name: SERVER.to_string(),
            })
            .await?;
        test.codex
            .submit(Op::ActivateMcpServer {
                server_name: SERVER.to_string(),
            })
            .await?;
        let response = completion(&server, "first").await;
        test.submit_turn("first real user turn").await?;
        let request = response.single_request();
        assert_eq!(request.tool_by_name(NAMESPACE, "echo").is_some(), implicit);
        assert_eq!(
            request
                .message_input_texts("developer")
                .iter()
                .filter(|text| text.contains("<mcp_use>"))
                .count(),
            usize::from(!implicit),
        );
        assert_eq!(
            test.codex
                .mcp_server_would_be_direct_at_session_start(SERVER)
                .await,
            implicit
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_activation_survives_completion_or_interrupt_without_an_extra_request() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    for (interrupt, queue_follow_up) in [(false, false), (false, true), (true, false)] {
        let (release, gate) = tokio::sync::oneshot::channel();
        let (streaming, _) = start_streaming_sse_server(vec![
            vec![
                StreamingSseChunk {
                    gate: None,
                    body: responses::sse(vec![responses::ev_response_created("active")]),
                },
                StreamingSseChunk {
                    gate: Some(gate),
                    body: responses::sse(vec![
                        responses::ev_assistant_message("active-answer", "first answer"),
                        responses::ev_completed("active"),
                    ]),
                },
            ],
            vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![
                    responses::ev_response_created("next"),
                    responses::ev_assistant_message("next-answer", "next answer"),
                    responses::ev_completed("next"),
                ]),
            }],
        ])
        .await;
        let server = responses::start_mock_server().await;
        let base_url = format!("{}/v1", streaming.uri());
        let test = test_codex()
            .with_config(move |config| config.model_provider.base_url = Some(base_url))
            .build_with_auto_env(&server)
            .await?;
        test.codex
            .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
                codex_protocol::user_input::UserInput::Text {
                    text: "start first turn".to_string(),
                    text_elements: Vec::new(),
                },
            ]))
            .await?;
        tokio::time::timeout(Duration::from_secs(10), streaming.wait_for_request_count(1))
            .await
            .context("first sampling request")?;
        // Core preserves the existing empty-inventory behavior when startup is unavailable.
        test.codex
            .submit(Op::ActivateMcpServer {
                server_name: "unavailable".to_string(),
            })
            .await?;
        let text = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(text) = test
                    .codex
                    .latest_mcp_server_use_context_text("unavailable")
                    .await
                {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("active activation boundary")?;
        if queue_follow_up {
            let outcome = test
                .codex
                .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
                    codex_protocol::user_input::UserInput::Text {
                        text: "queued next real user turn".to_string(),
                        text_elements: Vec::new(),
                    },
                ]))
                .await?;
            assert!(matches!(
                outcome,
                codex_protocol::turn_input::TurnInputSubmission::Steered { .. }
            ));
        }
        if interrupt {
            test.codex.submit(Op::Interrupt).await?;
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnAborted(_))
            })
            .await;
            let _ = release.send(());
        } else {
            release
                .send(())
                .map_err(|_| anyhow::anyhow!("stream gate closed"))?;
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnComplete(_))
            })
            .await;
        }
        if queue_follow_up {
            tokio::time::timeout(Duration::from_secs(10), streaming.wait_for_request_count(2))
                .await
                .context("deferred user input should start its next turn")?;
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnComplete(_))
            })
            .await;
        } else {
            assert_eq!(streaming.requests().await.len(), 1);
            test.submit_turn("next real user turn").await?;
        }
        let requests = streaming.requests().await;
        assert_eq!(requests.len(), 2);
        let body: serde_json::Value = serde_json::from_slice(&requests[1])?;
        let contexts = body["input"]
            .as_array()
            .context("request input")?
            .iter()
            .filter(|item| item["role"] == "developer")
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|content| content["text"].as_str())
            .filter(|text| text.contains("<mcp_use>"))
            .collect::<Vec<_>>();
        assert_eq!(contexts, vec![text.as_str()]);
        if queue_follow_up {
            let input = body["input"].as_array().context("input")?;
            let context_index = input
                .iter()
                .position(|item| {
                    item["content"].as_array().is_some_and(|content| {
                        content.iter().any(|part| {
                            part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("<mcp_use>"))
                        })
                    })
                })
                .context("explicit context position")?;
            let user_index = input
                .iter()
                .position(|item| {
                    item["role"] == "user"
                        && item["content"].as_array().is_some_and(|content| {
                            content
                                .iter()
                                .any(|part| part["text"] == "queued next real user turn")
                        })
                })
                .context("queued user input position")?;
            assert!(context_index < user_index);
        }
        streaming.shutdown().await;
    }
    Ok(())
}
