use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_image_generation_extension::install as install_image_generation_extension;
use codex_login::CodexAuth;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_web_search_extension::install as install_web_search_extension;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;

const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";
const V1_AGENT_TOOL_NAMES: &[&str] = &[
    SPAWN_AGENT_TOOL_NAME,
    "send_input",
    "wait_agent",
    "close_agent",
    "resume_agent",
];

fn responses_extensions(auth: &CodexAuth) -> Arc<ExtensionRegistry<Config>> {
    let auth_manager = codex_core::test_support::auth_manager_from_auth(auth.clone());
    let mut extension_builder = ExtensionRegistryBuilder::<Config>::new();
    install_web_search_extension(&mut extension_builder, Arc::clone(&auth_manager));
    install_image_generation_extension(&mut extension_builder, auth_manager, |config| {
        Some(config.codex_home.clone())
    });
    Arc::new(extension_builder.build())
}

fn configure_responses_tools(config: &mut Config) {
    assert!(config.web_search_mode.set(WebSearchMode::Live).is_ok());
    assert!(
        config
            .features
            .disable(Feature::StandaloneWebSearch)
            .is_ok()
    );
}

fn configure_image_capable_model(model_info: &mut codex_protocol::openai_models::ModelInfo) {
    model_info.input_modalities = vec![InputModality::Text, InputModality::Image];
}

fn has_hosted_tool(tools: &[Value], tool_type: &str) -> bool {
    tools
        .iter()
        .any(|tool| tool.get("type").and_then(Value::as_str) == Some(tool_type))
}

fn has_tool_name(tools: &[Value], tool_name: &str) -> bool {
    tools.iter().any(|tool| {
        tool.get("name")
            .or_else(|| tool.get("type"))
            .and_then(Value::as_str)
            == Some(tool_name)
    })
}

fn has_namespaced_tool(tools: &[Value], namespace: &str, tool_name: &str) -> bool {
    tools.iter().any(|tool| {
        tool.get("type").and_then(Value::as_str) == Some("namespace")
            && tool.get("name").and_then(Value::as_str) == Some(namespace)
            && tool["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
            })
    })
}

fn assert_lite_top_level_tools_are_only_tool_search(body: &Value) -> Result<()> {
    let Some(tools) = body.get("tools") else {
        return Ok(());
    };
    let tools = tools
        .as_array()
        .context("Responses Lite top-level tools should be an array")?;
    assert!(
        tools
            .iter()
            .all(|tool| tool.get("type").and_then(Value::as_str) == Some(TOOL_SEARCH_TOOL_NAME)),
        "Responses Lite should not expose regular tools at the top level: {tools:?}"
    );
    assert!(
        tools
            .iter()
            .all(|tool| tool.get("execution").and_then(Value::as_str) == Some("client")),
        "Responses Lite top-level tool_search should execute on the client: {tools:?}"
    );

    Ok(())
}

fn assert_lacks_v1_agent_tools(tools: &[Value], location: &str) {
    for tool_name in V1_AGENT_TOOL_NAMES {
        assert!(
            !has_tool_name(tools, tool_name),
            "V1 agent tool {tool_name} should not be exposed as a direct tool in {location}: {tools:?}"
        );
        assert!(
            !has_namespaced_tool(tools, MULTI_AGENT_V1_NAMESPACE, tool_name),
            "V1 agent tool {tool_name} should not be exposed as a namespace child in {location}: {tools:?}"
        );
    }
}

fn assert_lacks_v1_agent_tool_surfaces(body: &Value, location: &str) -> Result<()> {
    if let Some(tools) = body.get("tools") {
        let tools = tools
            .as_array()
            .with_context(|| format!("{location} top-level tools should be an array"))?;
        assert_lacks_v1_agent_tools(tools, &format!("{location} top-level tools"));
    }

    if let Some(input) = body.get("input") {
        let input = input
            .as_array()
            .with_context(|| format!("{location} input should be an array"))?;
        for item in input
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        {
            let tools = item["tools"]
                .as_array()
                .with_context(|| format!("{location} additional_tools should be an array"))?;
            assert_lacks_v1_agent_tools(tools, &format!("{location} additional_tools"));
        }
    }

    Ok(())
}

fn top_level_tools(body: &Value) -> Result<&[Value]> {
    body["tools"]
        .as_array()
        .map(Vec::as_slice)
        .context("Responses request top-level tools should be an array")
}

fn additional_tools(body: &Value) -> Result<&[Value]> {
    body["input"]
        .as_array()
        .context("Responses request input should be an array")?
        .first()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .context("Responses request should start with additional_tools")?["tools"]
        .as_array()
        .map(Vec::as_slice)
        .context("additional_tools tools should be an array")
}

fn value_contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value.contains(expected),
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_string(value, expected)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn request_body(request: &wiremock::Request) -> Option<Value> {
    serde_json::from_slice::<Value>(&request.body).ok()
}

fn request_message_contains(request: &wiremock::Request, role: &str, text: &str) -> bool {
    request_body(request)
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("role").and_then(Value::as_str) == Some(role)
                    && item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|content| {
                            content.iter().any(|span| {
                                span.get("type").and_then(Value::as_str) == Some("input_text")
                                    && span
                                        .get("text")
                                        .is_some_and(|value| value_contains_string(value, text))
                            })
                        })
            })
        })
}

fn request_has_call_output(request: &wiremock::Request, output_type: &str, call_id: &str) -> bool {
    request_body(request)
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some(output_type)
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

fn response_request_has_call_output(
    request: &responses::ResponsesRequest,
    output_type: &str,
    call_id: &str,
) -> bool {
    request
        .inputs_of_type(output_type)
        .iter()
        .any(|item| item.get("call_id").and_then(Value::as_str) == Some(call_id))
}

fn response_request_message_contains(
    request: &responses::ResponsesRequest,
    role: &str,
    text: &str,
) -> bool {
    request
        .message_input_texts(role)
        .iter()
        .any(|message| message.contains(text))
}

async fn wait_for_matching_request(
    mock: &responses::ResponseMock,
    mut predicate: impl FnMut(&responses::ResponsesRequest) -> bool,
    context: &str,
) -> Result<responses::ResponsesRequest> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(request) = mock
                .requests()
                .into_iter()
                .find(|request| predicate(request))
            {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for {context}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_uses_input_items_for_instructions_and_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.base_instructions = Some("test instructions".to_string());
        });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let body = response_mock.single_request().body_json();
    assert!(body.get("instructions").is_none());
    assert_lite_top_level_tools_are_only_tool_search(&body)?;

    let input = body["input"]
        .as_array()
        .context("Responses request input should be an array")?;
    assert_eq!(input[0]["type"], "additional_tools");
    assert_eq!(input[0]["role"], "developer");
    assert_eq!(
        input[1],
        serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [{
                "type": "input_text",
                "text": "test instructions",
            }],
        })
    );

    let tools = additional_tools(&body)?;
    assert!(!tools.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_tool_search_discovers_and_routes_v1_multi_agent_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let user_prompt = "Find the spawn agent tool";
    let child_prompt = "Inspect Responses Lite V1 routing.";
    let search_call_id = "tool-search-spawn-agent";
    let spawn_call_id = "spawn-agent-call";

    let search_mock = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request_message_contains(request, "user", user_prompt)
                && !request_has_call_output(request, "tool_search_output", search_call_id)
                && !request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_tool_search_call(
                search_call_id,
                &serde_json::json!({
                    "query": "spawn agent",
                    "limit": 1,
                }),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let spawn_args = serde_json::to_string(&serde_json::json!({
        "message": child_prompt,
    }))?;
    let spawn_call_output_id = spawn_call_id;
    let spawn_mock = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request_has_call_output(request, "tool_search_output", search_call_id)
                && !request_has_call_output(request, "function_call_output", spawn_call_output_id)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_function_call_with_namespace(
                spawn_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                SPAWN_AGENT_TOOL_NAME,
                &spawn_args,
            ),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let child_mock = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request_message_contains(request, "user", child_prompt)
                && !request_has_call_output(request, "tool_search_output", search_call_id)
                && !request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        responses::sse(vec![
            responses::ev_response_created("child-resp"),
            responses::ev_assistant_message("child-msg", "done"),
            responses::ev_completed("child-resp"),
        ]),
    )
    .await;

    let follow_up_mock = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            request_has_call_output(request, "tool_search_output", search_call_id)
                && request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            model_info.supports_search_tool = true;
            model_info.multi_agent_version = None;
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config
                .features
                .disable(Feature::EnableRequestCompression)
                .expect("test config should allow feature update");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn_with_approval_and_permission_profile(
        user_prompt,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let first_request = wait_for_matching_request(
        &search_mock,
        |request| {
            response_request_message_contains(request, "user", user_prompt)
                && !response_request_has_call_output(request, "tool_search_output", search_call_id)
                && !response_request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        "initial tool_search request",
    )
    .await?;
    assert_eq!(
        first_request.header(RESPONSES_LITE_HEADER).as_deref(),
        Some("true")
    );
    let first_body = first_request.body_json();
    assert!(first_body.get("instructions").is_none());
    assert_eq!(
        first_body
            .get("parallel_tool_calls")
            .and_then(Value::as_bool),
        Some(false)
    );

    let tools = top_level_tools(&first_body)?;
    assert_eq!(
        tools.len(),
        1,
        "Responses Lite should advertise only usable tool_search at the top level: {tools:?}"
    );
    let tool_search = tools
        .first()
        .context("Responses Lite should advertise tool_search at the top level")?;
    assert_eq!(
        tool_search.get("type").and_then(Value::as_str),
        Some(TOOL_SEARCH_TOOL_NAME)
    );
    assert_eq!(
        tool_search.get("execution").and_then(Value::as_str),
        Some("client")
    );
    assert_lacks_v1_agent_tools(tools, "top-level tools");

    let additional_tools = additional_tools(&first_body)?;
    assert!(
        !has_tool_name(additional_tools, TOOL_SEARCH_TOOL_NAME),
        "tool_search should not be hidden inside additional_tools: {additional_tools:?}"
    );
    assert_lacks_v1_agent_tools(additional_tools, "additional_tools");

    let search_output_request = wait_for_matching_request(
        &spawn_mock,
        |request| {
            response_request_has_call_output(request, "tool_search_output", search_call_id)
                && !response_request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        "request after tool_search_output",
    )
    .await?;
    let search_output_body = search_output_request.body_json();
    assert_lacks_v1_agent_tool_surfaces(&search_output_body, "tool_search_output request")?;
    let search_output = search_output_request.tool_search_output(search_call_id);
    assert_eq!(
        search_output.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert_eq!(
        search_output.get("execution").and_then(Value::as_str),
        Some("client")
    );
    let spawn_agent = responses::namespace_child_tool(
        &search_output,
        MULTI_AGENT_V1_NAMESPACE,
        SPAWN_AGENT_TOOL_NAME,
    )
    .expect("tool_search should return multi_agent_v1.spawn_agent");
    assert_eq!(
        spawn_agent.get("defer_loading").and_then(Value::as_bool),
        Some(true)
    );

    let child_request = wait_for_matching_request(
        &child_mock,
        |request| {
            response_request_message_contains(request, "user", child_prompt)
                && !response_request_has_call_output(request, "tool_search_output", search_call_id)
                && !response_request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        "V1 child turn request",
    )
    .await?;
    assert!(
        child_request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains(child_prompt)),
        "spawn_agent should route the requested child prompt into the V1 child turn: {:?}",
        child_request.input()
    );

    let follow_up_request = wait_for_matching_request(
        &follow_up_mock,
        |request| {
            response_request_has_call_output(request, "tool_search_output", search_call_id)
                && response_request_has_call_output(request, "function_call_output", spawn_call_id)
        },
        "request after spawn_agent output",
    )
    .await?;
    let follow_up_body = follow_up_request.body_json();
    assert_lacks_v1_agent_tool_surfaces(&follow_up_body, "spawn_agent output request")?;
    let spawn_output = follow_up_request.function_call_output(spawn_call_id);
    assert_eq!(
        spawn_output.get("call_id").and_then(Value::as_str),
        Some(spawn_call_id)
    );
    let output = spawn_output
        .get("output")
        .and_then(Value::as_str)
        .context("spawn_agent output should be serialized")?;
    let output: Value = serde_json::from_str(output)?;
    assert!(
        output.get("agent_id").and_then(Value::as_str).is_some(),
        "spawn_agent output should prove that the V1 handler ran: {output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_prepares_images() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let image_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let remote_image_url = "https://example.com/image.png";
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
        configure_image_capable_model(model_info);
    });
    let test = builder.build(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::Image {
                    image_url: image_url.to_string(),
                    detail: Some(ImageDetail::Original),
                },
                UserInput::Image {
                    image_url: remote_image_url.to_string(),
                    detail: Some(ImageDetail::High),
                },
            ],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response_mock.single_request();
    let user_content = request
        .input()
        .into_iter()
        .rev()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|item| item.get("content").and_then(Value::as_array).cloned())
        .context("request should contain user content")?;
    assert_eq!(
        user_content,
        vec![
            serde_json::json!({
                "type": "input_image",
                "image_url": image_url
            }),
            serde_json::json!({
                "type": "input_text",
                "text": "image content omitted because remote image URLs are not supported"
            }),
        ]
    );
    assert!(!request.body_json().to_string().contains(remote_image_url));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_uses_standalone_web_search_and_image_generation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let extensions = responses_extensions(&auth);

    let mut builder = test_codex()
        .with_auth(auth)
        .with_extensions(extensions)
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            configure_image_capable_model(model_info);
        })
        .with_config(configure_responses_tools);
    let test = builder.build(&server).await?;

    test.submit_turn("Use standalone tools").await?;

    let request = response_mock.single_request();
    assert_eq!(
        request.header(RESPONSES_LITE_HEADER).as_deref(),
        Some("true")
    );
    let body = request.body_json();
    assert_lite_top_level_tools_are_only_tool_search(&body)?;
    let tools = additional_tools(&body)?;
    assert!(has_namespaced_tool(tools, "web", "run"));
    assert!(has_namespaced_tool(tools, "image_gen", "imagegen"));
    assert!(!has_hosted_tool(tools, "web_search"));
    assert!(!has_hosted_tool(tools, "image_generation"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_exposes_standalone_tools_for_actor_authorized_provider() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let auth = CodexAuth::from_api_key("dummy");
    let extensions = responses_extensions(&auth);
    let mut builder = test_codex()
        .with_auth(auth)
        .with_extensions(extensions)
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            configure_image_capable_model(model_info);
        })
        .with_config(|config| {
            configure_responses_tools(config);
            config.model_provider.name = "local".to_string();
            config.model_provider.requires_openai_auth = false;
            config.model_provider.http_headers = Some(HashMap::from([(
                "x-openai-actor-authorization".to_string(),
                "test-actor-authorization".to_string(),
            )]));
        });
    let test = builder.build(&server).await?;

    test.submit_turn("Use standalone tools").await?;

    let body = response_mock.single_request().body_json();
    let tools = additional_tools(&body)?;
    assert!(has_namespaced_tool(tools, "web", "run"));
    assert!(has_namespaced_tool(tools, "image_gen", "imagegen"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_compact_request_uses_lite_transport_contract() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let compact_mock =
        responses::mount_compact_json_once(&server, serde_json::json!({ "output": [] })).await;

    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            model_info.supports_parallel_tool_calls = true;
        })
        .with_config(|config| {
            let _ = config.features.disable(Feature::RemoteCompactionV2);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("Compact this conversation").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    response_mock.single_request();
    let compact_request = compact_mock.single_request();
    assert_eq!(
        compact_request.header(RESPONSES_LITE_HEADER).as_deref(),
        Some("true")
    );
    let compact_body = compact_request.body_json();
    assert_eq!(
        compact_body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("context"))
            .and_then(Value::as_str),
        Some("all_turns")
    );
    assert_eq!(
        compact_body.get("parallel_tool_calls"),
        Some(&Value::Bool(false))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_omits_hosted_tools_without_standalone_extensions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            configure_image_capable_model(model_info);
        })
        .with_config(configure_responses_tools);
    let test = builder.build(&server).await?;

    test.submit_turn("Do not use hosted tools").await?;

    let body = response_mock.single_request().body_json();
    assert_lite_top_level_tools_are_only_tool_search(&body)?;
    let tools = additional_tools(&body)?;
    assert!(!has_hosted_tool(tools, "web_search"));
    assert!(!has_hosted_tool(tools, "image_generation"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_lite_uses_standalone_image_generation_by_default() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let extensions = responses_extensions(&auth);
    let mut builder = test_codex()
        .with_auth(auth)
        .with_extensions(extensions)
        .with_model_info_override("gpt-5.4", configure_image_capable_model)
        .with_config(configure_responses_tools);
    let test = builder.build(&server).await?;

    test.submit_turn("Use image generation").await?;

    let request = response_mock.single_request();
    assert_eq!(request.header(RESPONSES_LITE_HEADER), None);
    assert!(request.tool_by_name("web", "run").is_none());
    assert!(request.tool_by_name("image_gen", "imagegen").is_some());
    let body = request.body_json();
    let tools = body["tools"]
        .as_array()
        .context("Responses request tools should be an array")?;
    assert!(has_hosted_tool(tools, "web_search"));
    assert!(!has_hosted_tool(tools, "image_generation"));

    Ok(())
}
