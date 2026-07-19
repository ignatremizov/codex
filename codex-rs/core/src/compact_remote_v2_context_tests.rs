//! Context and MCP retention checks against the active remote-v2 pipeline.

use super::*;
use crate::compact::SUMMARY_PREFIX;
use crate::compact::build_compaction_replacement_history;
use crate::context::world_state::split_prefix_updates;
use crate::context_manager::updates::merge_world_state_updates;
use crate::session::PreviousTurnSettings;
use crate::session::tests::make_session_and_context;
use codex_context_fragments::ContextualUserFragment;
use pretty_assertions::assert_eq;

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn compaction_output() -> ResponseItem {
    ResponseItem::Compaction {
        id: None,
        encrypted_content: "remote-v2".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn raw(items: Vec<ResponseItemEnvelope>) -> Vec<ResponseItem> {
    items
        .into_iter()
        .map(ResponseItemEnvelope::into_item)
        .collect()
}

#[test]
fn client_developer_boundary_retains_text_within_its_serialized_budget() {
    let text = "client instructions ".repeat(500);
    let item = ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: None,
            status: None,
            encrypted_content: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        }),
    };
    for max_tokens in [128, 256] {
        let retained =
            truncate_retained_messages_for_remote_compaction(vec![item.clone()], max_tokens);
        let [retained] = retained.as_slice() else {
            panic!("the boundary must retain its budgeted client instructions");
        };
        assert!(estimate_item_token_count(&retained.item) <= max_tokens as i64);
        assert_eq!(retained.metadata, item.metadata);
        let ResponseItem::Message { role, content, .. } = &retained.item else {
            panic!("retained message");
        };
        assert_eq!(role, "developer");
        assert!(!content.is_empty());
    }
}

async fn rebuild_remote_history_with_test_session(
    input: Vec<ResponseItem>,
    previous_turn_settings: Option<&PreviousTurnSettings>,
) -> (Vec<ResponseItem>, Vec<ResponseItem>, Vec<ResponseItem>) {
    let (session, turn) = make_session_and_context().await;
    session
        .set_previous_turn_settings(previous_turn_settings.cloned())
        .await;
    let step_context = StepContext::for_test(Arc::new(turn));
    let world_state = Arc::new(
        session
            .build_world_state_for_step(&step_context, /*new_window*/ true)
            .await
            .expect("build captured compaction world state"),
    );
    let (updates, _) = session
        .build_initial_context_with_world_state(&step_context, &world_state)
        .await;
    let (prefix, context) = split_prefix_updates(updates);
    let context = merge_world_state_updates(context);
    let input = input
        .into_iter()
        .map(ResponseItemEnvelope::new)
        .collect::<Vec<_>>();
    let (retained, _) = build_v2_compacted_history(
        &input,
        /*compacted_prefix_len*/ 0,
        compaction_output(),
        session.features().get(),
    );
    let (history, _) =
        build_compaction_replacement_history(&session, &step_context, &world_state, retained).await;
    (raw(history), prefix, context)
}

#[test]
fn remote_v2_keeps_mcp_inventory_after_prompt_and_before_terminal_compaction() {
    let text =
        McpServerUseInstructions::new("linear".to_string(), r#"["linear"]"#.to_string()).render();
    let linear = ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let input = vec![user_message("retained prompt"), linear.clone()];
    let annotated = input
        .into_iter()
        .map(ResponseItemEnvelope::new)
        .collect::<Vec<_>>();
    let (merged, _) = build_v2_compacted_history(
        &annotated,
        /*compacted_prefix_len*/ 0,
        compaction_output(),
        &Features::default(),
    );
    assert_eq!(
        raw(merged),
        vec![user_message("retained prompt"), linear, compaction_output()]
    );
}

#[tokio::test]
async fn remote_v2_context_replaces_developer_messages() {
    let compacted_history = vec![
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "stale permissions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "stale personality".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let (refreshed, mut expected, context) = rebuild_remote_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    expected.extend(context);
    expected.push(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    expected.push(compaction_output());
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn remote_v2_context_reinjects_full_initial_context() {
    let compacted_history = vec![ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let (refreshed, mut expected, context) = rebuild_remote_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    expected.extend(context);
    expected.push(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    expected.push(compaction_output());
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn remote_v2_context_drops_non_user_content_messages() {
    let compacted_history = vec![
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"# AGENTS.md instructions for /repo

<INSTRUCTIONS>
keep me updated
</INSTRUCTIONS>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"<environment_context>
  <cwd>/repo</cwd>
  <shell>zsh</shell>
</environment_context>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"<turn_aborted>
  <turn_id>turn-1</turn_id>
  <reason>interrupted</reason>
</turn_aborted>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "stale developer instructions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let (refreshed, mut expected, context) = rebuild_remote_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    expected.extend(context);
    expected.push(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    expected.push(compaction_output());
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn remote_v2_context_drops_legacy_warnings() {
    let latest_user = user_message("latest user");
    let compacted_history = vec![
        user_message(
            "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        ),
        user_message(
            "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.",
        ),
        user_message(
            "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety",
        ),
        latest_user.clone(),
    ];
    let (refreshed, mut expected, context) = rebuild_remote_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    expected.extend(context);
    expected.push(latest_user);
    expected.push(compaction_output());
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn remote_v2_context_inserts_context_before_last_real_user_message_only() {
    let compacted_history = vec![
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "latest user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let (refreshed, mut expected, context) = rebuild_remote_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    expected.extend([
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ]);
    expected.extend(context);
    expected.push(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "latest user".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    expected.push(compaction_output());
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn remote_v2_context_reinjects_model_switch_message() {
    let compacted_history = vec![ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let previous_turn_settings = PreviousTurnSettings {
        model: "previous-regular-model".to_string(),
        cyber_access_program: None,
        comp_hash: None,
        realtime_active: None,
    };

    let (refreshed, mut expected, context) =
        rebuild_remote_history_with_test_session(compacted_history, Some(&previous_turn_settings))
            .await;

    let ResponseItem::Message { role, content, .. } = &context[0] else {
        panic!("expected developer message");
    };
    assert_eq!(role, "developer");
    let [ContentItem::InputText { text }, ..] = content.as_slice() else {
        panic!("expected developer text");
    };
    assert!(text.contains("<model_switch>"));

    expected.extend(context);
    expected.push(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    expected.push(compaction_output());
    assert_eq!(refreshed, expected);
}
