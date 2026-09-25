use super::*;
use crate::context::AgentContextIdentity;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;
use codex_code_mode::CellId;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::ExecutedToolCall;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_tools::ToolName;
use codex_utils_output_truncation::approx_token_count;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[tokio::test]
async fn repeated_local_compaction_preserves_every_explicit_mcp_inventory() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let provider = ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    let (session, turn, _events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        Vec::new(),
        move |config| {
            config.model = Some("gpt-5.2".to_string());
            config.model_provider = provider;
            config.model_provider.supports_websockets = false;
        },
    )
    .await;
    let mut items = vec![user_message("first prompt")];
    for (server_name, inventory) in [
        (
            "linear",
            json!([{"description": "old ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS + 1)}])
                .to_string(),
        ),
        ("github", r#"["issues"]"#.to_string()),
        ("linear", r#"["updated"]"#.to_string()),
    ] {
        items.push(ContextualUserFragment::into(McpServerUseInstructions::new(
            server_name.to_string(),
            inventory,
        )));
        items.push(user_message("next prompt"));
    }
    items.push(ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "ordinary developer context".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    session
        .record_conversation_items(&turn, turn.model_info(), &items)
        .await;
    let expected = session
        .clone_history()
        .await
        .annotated_items()
        .iter()
        .filter(|envelope| McpServerUseInstructions::matches_response_item(&envelope.item))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 3);
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("summary-1", "First summary."),
                responses::ev_completed("compact-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("summary-2", "Second summary."),
                responses::ev_completed("compact-2"),
            ]),
        ],
    )
    .await;
    for _ in 0..2 {
        run_compact_task(
            Arc::clone(&session),
            Arc::clone(&turn),
            vec![UserInput::Text {
                text: "Summarize the conversation.".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await?;
        let history = session.clone_history().await;
        let developers = history
            .annotated_items()
            .iter()
            .filter(|envelope| {
                matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "developer")
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(developers, expected);
        assert_eq!(
            &history.annotated_items()[..expected.len()],
            expected.as_slice()
        );
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let explicit_context = request
            .input()
            .into_iter()
            .filter_map(|item| serde_json::from_value::<ResponseItem>(item).ok())
            .filter(McpServerUseInstructions::matches_response_item)
            .collect::<Vec<_>>();
        assert_eq!(
            explicit_context,
            expected
                .iter()
                .map(|envelope| envelope.item.clone())
                .collect::<Vec<_>>(),
        );
    }
    Ok(())
}

#[test_case(true; "metadata enabled")]
#[test_case(false; "metadata disabled after capture")]
#[tokio::test]
async fn local_compaction_respects_tool_metadata_state(
    metadata_enabled: bool,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let provider = ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    let (session, turn, _events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        Vec::new(),
        move |config| {
            config.model = Some("gpt-5.2".to_string());
            config.model_provider = provider;
            config.model_provider.supports_websockets = false;
            config
                .features
                .enable(Feature::ExecutedToolCallMetadata)
                .expect("enable tool-call metadata");
        },
    )
    .await;

    let mut items = vec![user_message("Update the plan")];
    for index in 0..5 {
        let call_id = format!("direct-{index}");
        let arguments = json!({"plan": [{"step": "x".repeat(7 * 1024), "status": "completed"}]});
        assert!(serde_json::to_vec(&arguments)?.len() < 8 * 1024);
        items.push(ResponseItem::FunctionCall {
            id: None,
            name: "update_plan".to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            encrypted_function_args: None,
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        });
        let mut output = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("Plan updated".to_string()),
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    turn_id: Some("metadata-canary".to_string()),
                    create_time: Some(123.into()),
                    ..Default::default()
                },
            ),
        };
        output.append_executed_tool_calls(vec![ExecutedToolCall::new(
            "update_plan".to_string(),
            arguments,
        )]);
        output.mark_tool_calls_complete();
        items.push(output);
    }
    let cell = CellId::new("local-compaction-cell".to_string());
    let nested_call = ExecutedToolCall::new("nested_tool".to_string(), json!({}));
    let recorder = &session.services.executed_tool_calls;
    recorder.start_cell(&cell, "exec");
    recorder.record_tool_call(
        &ToolCall {
            tool_name: ToolName::plain("nested_tool"),
            call_id: "nested".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            encrypted_function_args: None,
        },
        &ToolCallSource::CodeMode {
            cell_id: cell.as_str().to_string(),
            runtime_tool_call_id: "nested".to_string(),
        },
        &StepContext::for_test(Arc::clone(&turn)),
    );
    recorder.finish_cell_recording(&cell);
    items.push(serde_json::from_value(json!({
        "type": "custom_tool_call", "call_id": "exec", "name": "exec", "input": "",
    }))?);
    items.push(serde_json::from_value(json!({
        "type": "custom_tool_call_output", "call_id": "exec", "output": "done",
    }))?);
    session
        .record_conversation_items(&turn, turn.model_info(), &items)
        .await;
    let live_history = session.clone_history().await;
    let mut expected_code_mode_output = live_history
        .raw_items()
        .find(|item| matches!(item, ResponseItem::CustomToolCallOutput { .. }))
        .expect("Code Mode output recorded")
        .clone();
    if metadata_enabled {
        expected_code_mode_output.append_executed_tool_calls(vec![nested_call]);
        expected_code_mode_output.set_tool_call_cell_id("exec");
        expected_code_mode_output.mark_tool_calls_complete();
    }
    let outputs = live_history
        .raw_items()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput { .. } => Some(serde_json::to_value(item)),
            _ => None,
        })
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(outputs.len(), 5);
    let metadata_bytes: usize = outputs
        .iter()
        .map(|item| {
            serde_json::to_vec(&item["internal_chat_message_metadata_passthrough"])
                .unwrap()
                .len()
        })
        .sum();
    // Compaction does not rebudget source records as a normal inference request.
    // Passthrough bytes are also excluded from model token estimates.
    assert!(metadata_bytes > 32 * 1024);

    if !metadata_enabled {
        let mut config = (*session.get_config().await).clone();
        config.features.disable(Feature::ExecutedToolCallMetadata)?;
        session.refresh_runtime_config(config).await;
    }

    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("summary", "The prior calls finished."),
            responses::ev_completed("compact-response"),
        ]),
    )
    .await;
    // OpenAI identity keeps the client from removing passthrough for compatibility.
    run_compact_task(
        Arc::clone(&session),
        turn,
        vec![UserInput::Text {
            text: "Summarize the conversation.".to_string(),
            text_elements: Vec::new(),
        }],
    )
    .await?;

    let request = mock.single_request();
    assert!(request.inputs_of_type("compaction_trigger").is_empty());
    assert_eq!(
        request.custom_tool_call_output("exec"),
        serde_json::to_value(expected_code_mode_output)?,
    );
    for mut output in outputs {
        let call_id = output["call_id"].as_str().expect("source call id");
        let compact_output = request.function_call_output(call_id);
        assert_eq!(compact_output["output"], json!("Plan updated"));
        if !metadata_enabled {
            let metadata = output["internal_chat_message_metadata_passthrough"]
                .as_object_mut()
                .expect("source metadata");
            metadata.remove("executed_tool_calls");
            metadata.remove("tool_calls_complete");
        }
        assert_eq!(
            compact_output["internal_chat_message_metadata_passthrough"],
            output["internal_chat_message_metadata_passthrough"]
        );
    }
    let compacted_history = session.clone_history().await;
    assert!(compacted_history.raw_items().any(|item| {
        matches!(item, ResponseItem::Message { role, content, .. }
            if role == "user"
                && content_items_to_text(content).as_deref()
                    .and_then(summary_for_event).as_deref() == Some("The prior calls finished."))
    }));
    Ok(())
}

fn annotated(items: Vec<ResponseItem>) -> Vec<ResponseItemEnvelope> {
    items.into_iter().map(ResponseItemEnvelope::new).collect()
}

fn raw(items: Vec<ResponseItemEnvelope>) -> Vec<ResponseItem> {
    items
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect()
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn compaction_summary_message(text: &str) -> ResponseItem {
    ContextualUserFragment::into(CompactionSummary::new(text))
}

fn developer_message(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn persistent_agent_reply_route(agent_id: ThreadId, nickname: &str) -> ResponseItem {
    ContextualUserFragment::into(AgentReplyRoute::until_disabled(AgentContextIdentity::V1 {
        agent_id,
        agent_ref: Some(2),
        nickname: Some(nickname.to_string()),
        task_path: None,
    }))
}

#[test]
fn persistent_route_compaction_keeps_one_latest_envelope_without_pinning_client_text() {
    let source = ThreadId::new();
    let old = persistent_agent_reply_route(source, "Old");
    let current = persistent_agent_reply_route(source, "Current");
    let mut quoted = ResponseItemEnvelope::new(current.clone());
    quoted.metadata = Some(CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let history = vec![old.clone().into(), current.clone().into(), quoted.clone()];
    let routes = persistent_agent_reply_routes(&history);
    assert_eq!(routes, vec![ResponseItemEnvelope::new(current.clone())]);
    let prompt = ResponseItemEnvelope::new(user_message("retained prompt"));
    assert_eq!(
        insert_initial_context_before_last_real_user_or_summary(
            vec![old.into(), quoted.clone(), prompt.clone()],
            routes,
        ),
        vec![quoted, ResponseItemEnvelope::new(current), prompt],
    );
}

fn compacted_user_message(text: &str) -> CompactedUserMessage {
    CompactedUserMessage {
        id: None,
        message: text.to_string(),
        internal_chat_message_metadata_passthrough: None,
        harness_metadata: None,
    }
}

#[test]
fn content_items_to_text_joins_non_empty_segments() {
    let items = vec![
        ContentItem::InputText {
            text: "hello".to_string(),
        },
        ContentItem::OutputText {
            text: String::new(),
        },
        ContentItem::OutputText {
            text: "world".to_string(),
        },
    ];

    let joined = content_items_to_text(&items);

    assert_eq!(Some("hello\nworld".to_string()), joined);
}

#[test]
fn content_items_to_text_ignores_image_only_content() {
    let items = vec![ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "file://image.png".to_string(),
        },
        detail: Some(DEFAULT_IMAGE_DETAIL),
    }];

    let joined = content_items_to_text(&items);

    assert_eq!(None, joined);
}

#[test]
fn collect_user_messages_extracts_user_text_only() {
    let items = vec![
        ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "assistant")),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "ignored".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "user")),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "first".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Other,
    ];

    let collected = collect_user_messages(&items);

    assert_eq!(
        vec![CompactedUserMessage {
            id: Some(ResponseItemId::with_suffix("msg", "user")),
            ..compacted_user_message("first")
        }],
        collected,
    );
}

#[test]
fn collect_annotated_user_messages_extracts_user_text_only() {
    let items = vec![
        ResponseItemEnvelope {
            item: user_message("first"),
            metadata: Some(CodexHarnessMetadata::default()),
        },
        ResponseItemEnvelope::new(ResponseItem::Other),
    ];

    let collected = collect_annotated_user_messages(&items, CompactedMessageIdentity::Preserve);

    assert_eq!(
        vec![CompactedUserMessage {
            id: None,
            message: "first".to_string(),
            internal_chat_message_metadata_passthrough: None,
            harness_metadata: Some(CodexHarnessMetadata::default()),
        }],
        collected
    );
}

#[test]
fn collect_user_messages_filters_session_prefix_entries() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"# AGENTS.md instructions for project

<INSTRUCTIONS>
do things
</INSTRUCTIONS>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "<ENVIRONMENT_CONTEXT>cwd=/tmp</ENVIRONMENT_CONTEXT>".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "real user message".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let collected = collect_user_messages(&items);

    assert_eq!(vec![compacted_user_message("real user message")], collected);
}

#[test]
fn collect_user_messages_filters_legacy_warnings() {
    let items = vec![
        user_message(
            "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        ),
        user_message(
            "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.",
        ),
        user_message(
            "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety",
        ),
        user_message("real user message"),
    ];

    let collected = collect_user_messages(&items);

    assert_eq!(vec![compacted_user_message("real user message")], collected);
}

#[test]
fn collect_user_messages_filters_turn_aborted_marker() {
    let items = vec![
        user_message(
            "<turn_aborted>\n  <turn_id>turn-1</turn_id>\n  <reason>interrupted</reason>\n</turn_aborted>",
        ),
        user_message("real user message"),
    ];

    let collected = collect_user_messages(&items);

    assert_eq!(vec![compacted_user_message("real user message")], collected);
}

#[test]
fn build_token_limited_compacted_history_truncates_overlong_user_messages() {
    // Use a small truncation limit so the test remains fast while still validating
    // that oversized user content is truncated.
    let max_tokens = 16;
    let big = "word ".repeat(200);
    let user_message = CompactedUserMessage {
        id: Some(ResponseItemId::with_suffix("msg", "long-user")),
        message: big.clone(),
        internal_chat_message_metadata_passthrough: None,
        harness_metadata: Some(CodexHarnessMetadata::default()),
    };
    let history = super::build_compacted_history_with_limit(
        Vec::new(),
        std::slice::from_ref(&user_message),
        "SUMMARY",
        max_tokens,
    );
    assert_eq!(history.len(), 2);

    let truncated_message = &history[0].item;
    let summary_message = &history[1].item;

    let truncated_text = match truncated_message {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("unexpected item in history: {other:?}"),
    };

    assert!(
        truncated_text.contains("tokens truncated"),
        "expected truncation marker in truncated user message"
    );
    assert!(
        !truncated_text.contains(&big),
        "truncated user message should not include the full oversized user text"
    );

    let summary_text = match summary_message {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("unexpected item in history: {other:?}"),
    };
    assert_eq!(summary_text, "SUMMARY");
    assert_eq!(history[0].id(), user_message.id.as_ref());
    assert_eq!(history[0].metadata, Some(CodexHarnessMetadata::default()));
    assert_eq!(history[1].metadata, None);
}

#[test]
fn build_token_limited_compacted_history_appends_summary_message() {
    let initial_context: Vec<ResponseItemEnvelope> = Vec::new();
    let user_messages = vec![compacted_user_message("first user message")];
    let summary_text = "summary text";

    let history = build_compacted_history(initial_context, &user_messages, summary_text);
    assert!(
        !history.is_empty(),
        "expected compacted history to include summary"
    );

    let last = history.last().expect("history should have a summary entry");
    let summary = match &last.item {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("expected summary message, found {other:?}"),
    };
    assert_eq!(summary, summary_text);
}

#[test]
fn build_compacted_history_preserves_user_message_passthrough_metadata() {
    let history = build_compacted_history(
        Vec::new(),
        &[CompactedUserMessage {
            id: Some(ResponseItemId::with_suffix("msg", "user")),
            message: "first user message".to_string(),
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    turn_id: Some("turn-1".to_string()),
                    content_item_kinds: Some(vec![
                        ContentItemKind("user.image".to_string()),
                        ContentItemKind("user.text".to_string()),
                        ContentItemKind("user.audio".to_string()),
                    ]),
                    ..Default::default()
                },
            ),
            harness_metadata: Some(CodexHarnessMetadata::default()),
        }],
        "summary text",
    );

    assert_eq!(
        history,
        vec![
            ResponseItemEnvelope {
                item: ResponseItem::Message {
                    id: Some(ResponseItemId::with_suffix("msg", "user")),
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "first user message".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: Some(
                        InternalChatMessageMetadataPassthrough {
                            turn_id: Some("turn-1".to_string()),
                            content_item_kinds: Some(vec![ContentItemKind(
                                "user.text".to_string()
                            )]),
                            ..Default::default()
                        },
                    ),
                },
                metadata: Some(CodexHarnessMetadata::default()),
            },
            ResponseItemEnvelope::new(ContextualUserFragment::into(CompactionSummary::new(
                "summary text",
            ))),
        ]
    );
}

#[test]
fn build_compacted_history_preserving_mcp_context_keeps_invocation_order_in_retained_tail() {
    let linear =
        McpServerUseInstructions::new("linear".to_string(), r#"["linear"]"#.to_string()).render();
    let history = vec![
        user_message("first prompt"),
        developer_message(linear.clone()),
        user_message("second prompt"),
    ];

    let compacted = build_compacted_history_preserving_mcp_context(
        &annotated(history),
        "summary text",
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    );

    assert_eq!(
        raw(compacted),
        vec![
            user_message("first prompt"),
            developer_message(linear),
            user_message("second prompt"),
            compaction_summary_message("summary text"),
        ]
    );
}

#[test]
fn build_local_compacted_history_expires_flattened_inherited_image_paths() {
    let inherited_path = "/tmp/inherited.png";
    let inherited = user_message(&format!(
        "before<image name=[Image #1] path=\"{inherited_path}\">image omitted</image>after"
    ));
    let current = user_message("current window");

    let compacted = build_local_compacted_history(
        &annotated(vec![inherited, current.clone()]),
        /*compacted_prefix_len*/ 1,
        "summary text",
    );

    assert_eq!(
        raw(compacted),
        vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "beforeafter".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        content_item_kinds: Some(vec![ContentItemKind("user.text".to_string())]),
                        ..Default::default()
                    },
                ),
            },
            current,
            compaction_summary_message("summary text"),
        ]
    );
}

#[test]
fn build_local_compacted_history_retains_current_image_path_for_one_window() {
    let current_path = "/tmp/current.png";
    let current = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "before".to_string(),
            },
            ContentItem::InputText {
                text: format!("<image name=[Image #1] path=\"{current_path}\">"),
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,current".to_string(),
                },
                detail: None,
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
            ContentItem::InputText {
                text: "after".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let first_summary = format!("{SUMMARY_PREFIX}\nfirst summary");
    let first = build_local_compacted_history(
        &annotated(vec![current]),
        /*compacted_prefix_len*/ 0,
        &first_summary,
    );
    let expected_retained = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "before".to_string(),
            },
            ContentItem::InputText {
                text: format!("<image name=[Image #1] path=\"{current_path}\">"),
            },
            ContentItem::InputText {
                text: crate::context::CompactedImageOmission::reopenable_local_image().render(),
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
            ContentItem::InputText {
                text: "after".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            content_item_kinds: Some(vec![
                ContentItemKind("unknown".to_string()),
                ContentItemKind("unknown".to_string()),
                ContentItemKind("compaction.image_omission".to_string()),
                ContentItemKind("unknown".to_string()),
                ContentItemKind("unknown".to_string()),
            ]),
            ..Default::default()
        }),
    };
    assert_eq!(
        raw(first.clone()),
        vec![
            expected_retained,
            compaction_summary_message(&first_summary),
        ]
    );

    let mut second_input = first;
    let second_prefix_len = second_input.len();
    second_input.push(user_message("next window").into());
    let second_summary = format!("{SUMMARY_PREFIX}\nsecond summary");
    let second = build_local_compacted_history(&second_input, second_prefix_len, &second_summary);

    assert_eq!(
        raw(second),
        vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "beforeafter".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        content_item_kinds: Some(vec![ContentItemKind("user.text".to_string())]),
                        ..Default::default()
                    },
                ),
            },
            user_message("next window"),
            compaction_summary_message(&second_summary),
        ]
    );
}

#[test]
fn build_local_compacted_history_drops_oversized_image_message_atomically() {
    let huge_text = "x".repeat((COMPACT_USER_MESSAGE_MAX_TOKENS + 1) * 4);
    let current = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: huge_text.clone(),
            },
            ContentItem::InputText {
                text: "<image name=[Image #1] path=\"/tmp/current.png\">".to_string(),
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,current".to_string(),
                },
                detail: None,
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
            ContentItem::InputText { text: huge_text },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let summary = format!("{SUMMARY_PREFIX}\nsummary");

    let compacted = build_local_compacted_history(
        &annotated(vec![current]),
        /*compacted_prefix_len*/ 0,
        &summary,
    );

    assert_eq!(
        raw(compacted),
        vec![
            crate::context::standalone_compacted_image_omission_message(
                crate::context::CompactedImageOmission::reopenable_local_image().render()
            ),
            compaction_summary_message(&summary),
        ]
    );
}

#[test]
fn build_local_compacted_history_rehomes_tool_only_image_omission() {
    let current = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("tool-call".to_string()),
        name: None,
        namespace: None,
        output: codex_protocol::models::FunctionCallOutputPayload::from_content_items(vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,current".to_string(),
                },
                detail: None,
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };
    let summary = format!("{SUMMARY_PREFIX}\nsummary");

    let compacted = build_local_compacted_history(
        &annotated(vec![current]),
        /*compacted_prefix_len*/ 0,
        &summary,
    );

    assert_eq!(
        raw(compacted),
        vec![
            crate::context::standalone_compacted_image_omission_message(
                crate::context::CompactedImageOmission::unavailable().render()
            ),
            compaction_summary_message(&summary),
        ]
    );
}

#[test]
fn build_local_compacted_history_charges_rehomed_omission_to_retention_budget() {
    let history = vec![
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("tool-call".to_string()),
            name: None,
            namespace: None,
            output: codex_protocol::models::FunctionCallOutputPayload::from_content_items(vec![
                codex_protocol::models::FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,current".to_string(),
                    },
                    detail: None,
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        user_message(
            "x".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS.saturating_mul(4))
                .as_str(),
        ),
    ];
    let summary = format!("{SUMMARY_PREFIX}\nsummary");

    let compacted = build_local_compacted_history(
        &annotated(history),
        /*compacted_prefix_len*/ 0,
        &summary,
    );
    let retained = raw(compacted[..compacted.len().saturating_sub(1)].to_vec());
    let retained_tokens = retained
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => content_items_to_text(content),
            _ => None,
        })
        .map(|text| approx_token_count(text.as_str()))
        .sum::<usize>();

    assert!(retained_tokens <= COMPACT_USER_MESSAGE_MAX_TOKENS);
    assert!(compacted_image_omission_text(&retained).is_some());
}

#[test]
fn build_local_compacted_history_drops_oversized_text_image_wrapper_atomically() {
    let huge_text = "x".repeat((COMPACT_USER_MESSAGE_MAX_TOKENS + 1) * 4);
    let current = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: huge_text.clone(),
            },
            ContentItem::InputText {
                text: "<image name=[Image #1] path=\"/tmp/current.png\">".to_string(),
            },
            ContentItem::InputText {
                text: "[Image #1]".to_string(),
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
            ContentItem::InputText {
                text: "after".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let summary = format!("{SUMMARY_PREFIX}\nsummary");
    let expected = crate::compacted_history_retention::truncate_text_to_approx_token_budget(
        &huge_text,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    );

    let compacted = build_local_compacted_history(
        &annotated(vec![current]),
        /*compacted_prefix_len*/ 0,
        &summary,
    );

    assert_eq!(
        raw(compacted),
        vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: expected }],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        content_item_kinds: Some(vec![ContentItemKind("unknown".to_string())]),
                        ..Default::default()
                    },
                ),
            },
            compaction_summary_message(&summary),
        ]
    );
}

#[test]
fn build_local_compacted_history_preserves_whole_message_text_truncation() {
    let leading = "x".repeat((COMPACT_USER_MESSAGE_MAX_TOKENS + 1) * 4);
    let unmatched_opener = "<image name=[Image #1] path=\"/tmp/not-a-wrapper.png\">";
    let trailing = "TRAILING_SENTINEL";
    let current = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: leading.clone(),
            },
            ContentItem::InputText {
                text: unmatched_opener.to_string(),
            },
            ContentItem::InputText {
                text: trailing.to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let summary = format!("{SUMMARY_PREFIX}\nsummary");
    let expected = crate::compacted_history_retention::truncate_text_to_approx_token_budget(
        &format!("{leading}{unmatched_opener}{trailing}"),
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    );

    let compacted = build_local_compacted_history(
        &annotated(vec![current]),
        /*compacted_prefix_len*/ 0,
        &summary,
    );

    assert!(expected.ends_with(trailing));
    assert_eq!(
        raw(compacted),
        vec![
            user_message(&expected),
            compaction_summary_message(&summary),
        ]
    );
}

#[test]
fn local_compaction_keeps_mcp_inventory_after_prompt_and_before_summary() {
    let linear =
        McpServerUseInstructions::new("linear".to_string(), r#"["linear"]"#.to_string()).render();
    let summary = format!("{SUMMARY_PREFIX}\nsummary text");
    let history = annotated(vec![
        user_message("retained prompt"),
        developer_message(linear.clone()),
    ]);
    let merged = raw(build_compacted_history_preserving_mcp_context(
        &history,
        &summary,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    ));

    assert_eq!(
        merged
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { role, content, .. } => {
                    Some(format!("{role}:{}", content_items_to_text(content)?))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![
            "user:retained prompt".to_string(),
            format!("developer:{linear}"),
            format!("user:{summary}"),
        ]
    );
}

#[test]
fn insert_initial_context_before_last_real_user_or_summary_keeps_summary_last() {
    let agent_completion = ResponseItem::AgentMessage {
        id: None,
        author: "child".to_string(),
        recipient: "parent".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "Message Type: FINAL_ANSWER\nPayload:\nchild completion".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "latest user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        agent_completion.clone(),
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let initial_context = vec![ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "fresh permissions".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let refreshed = raw(insert_initial_context_before_last_real_user_or_summary(
        annotated(compacted_history),
        annotated(initial_context),
    ));
    let expected = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "fresh permissions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "latest user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        agent_completion,
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(refreshed, expected);
}

#[test]
fn insert_initial_context_before_last_real_user_or_summary_keeps_compaction_last() {
    let agent_task = ResponseItem::AgentMessage {
        id: None,
        author: "parent".to_string(),
        recipient: "child".to_string(),
        content: Vec::new(),
        internal_chat_message_metadata_passthrough: None,
    };
    let compacted_history = vec![
        agent_task.clone(),
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let initial_context = vec![ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "fresh permissions".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let refreshed = raw(insert_initial_context_before_last_real_user_or_summary(
        annotated(compacted_history),
        annotated(initial_context),
    ));
    let expected = vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "fresh permissions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        agent_task,
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(refreshed, expected);
}
