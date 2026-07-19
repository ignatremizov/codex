//! Context and MCP retention checks against the active remote-v2 pipeline.

use super::*;
use crate::compact::SUMMARY_PREFIX;
use crate::compact::build_compaction_replacement_history;
use crate::context::world_state::split_prefix_updates;
use crate::context_manager::updates::merge_world_state_updates;
use crate::session::PreviousTurnSettings;
use crate::session::tests::make_session_and_context;
use codex_context_fragments::ContextualUserFragment;
use codex_protocol::models::AudioReference;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ContentItemMetadata;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::image_close_tag_text;
use codex_protocol::models::local_image_open_tag_text_with_path;
use codex_protocol_test_support::ContentItemTestExt;
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

#[test]
fn remote_v2_sanitization_preserves_image_labels_audio_and_annotations() {
    let image_url = "data:image/png;base64,abc";
    for image in [
        ImageReference::Inline {
            image_url: image_url.to_string(),
        },
        ImageReference::File {
            file_id: "file_123".to_string(),
        },
    ] {
        let expected_inline_bytes = if matches!(&image, ImageReference::Inline { .. }) {
            image_url.len() as u64
        } else {
            0
        };
        let content = vec![
            ContentItem::InputText {
                text: "earlier text".to_string(),
            },
            ContentItem::InputText {
                text: local_image_open_tag_text_with_path(7, std::path::Path::new("image.png")),
            },
            ContentItem::InputImage {
                image,
                detail: Some(codex_protocol::models::ImageDetail::Original),
            },
            ContentItem::InputText {
                text: image_close_tag_text(),
            },
            ContentItem::InputText {
                text: "keep".to_string(),
            },
            ContentItem::InputAudio {
                audio: AudioReference::Inline {
                    audio_url: "data:audio/wav;base64,abc".to_string(),
                },
            },
            ContentItem::InputAudio {
                audio: AudioReference::File {
                    file_id: "file_audio".to_string(),
                },
            },
        ];
        let kinds = (0..content.len())
            .map(|index| ContentItemKind(format!("part.{index}")))
            .collect::<Vec<_>>();
        let mut source = ResponseItemEnvelope {
            item: user_message(""),
            metadata: Some(CodexHarnessMetadata {
                user_input_order: Some(17),
                inherited_user_message: true,
                ..Default::default()
            }),
        };
        let ResponseItem::Message {
            content: source_content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut source.item
        else {
            unreachable!();
        };
        *source_content = content;
        *internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                turn_id: Some("turn-1".to_string()),
                create_time: Some(1234.into()),
                content_item_metadata: Some(vec![ContentItemMetadata::user(); kinds.len()]),
                content_item_kinds: Some(kinds),
                ..Default::default()
            });
        let mut expected = source.clone();
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut expected.item
        else {
            unreachable!();
        };
        content[2] = ContentItem::InputText {
            text: crate::context::CompactedImageOmission::reopenable_local_image()
                .render()
                .expect_text(),
        };
        let metadata = internal_chat_message_metadata_passthrough.as_mut().unwrap();
        metadata.content_item_kinds.as_mut().unwrap()[2] =
            ContentItemKind("compaction.image_omission".to_string());
        metadata.content_item_metadata.as_mut().unwrap()[2] = ContentItemMetadata::harness();

        let (retained, sanitization) = build_v2_compacted_history(
            &[source],
            /*compacted_prefix_len*/ 0,
            compaction_output(),
            &Features::default(),
        );

        assert!(sanitization.changed());
        assert_eq!(sanitization.omitted_image_count, 1);
        assert_eq!(
            sanitization.omitted_inline_media_bytes,
            expected_inline_bytes
        );
        assert_eq!(
            retained,
            vec![expected, ResponseItemEnvelope::new(compaction_output())]
        );
    }
}

#[test]
fn sanitized_image_boundary_is_atomic_without_backfilling_older_messages() {
    let opener = local_image_open_tag_text_with_path(7, std::path::Path::new("image.png"));
    let omission = crate::context::CompactedImageOmission::reopenable_local_image()
        .render()
        .expect_text();
    let close = image_close_tag_text();
    let reference_tokens =
        approx_token_count(&opener) + approx_token_count(&omission) + approx_token_count(&close);
    let mut image_message = user_message("");
    let ResponseItem::Message { content, .. } = &mut image_message else {
        unreachable!();
    };
    *content = vec![
        ContentItem::InputText { text: opener },
        ContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,abc".to_string(),
            },
            detail: None,
        },
        ContentItem::InputText { text: close },
    ];
    let old = ResponseItemEnvelope::new(user_message("old"));
    let newest = ResponseItemEnvelope::new(user_message("new"));
    let (retained, _) = build_v2_compacted_history(
        &[
            old.clone(),
            ResponseItemEnvelope::new(image_message),
            newest.clone(),
        ],
        /*compacted_prefix_len*/ 0,
        compaction_output(),
        &Features::default(),
    );
    let [actual_old, reference, actual_newest, terminal] = retained.as_slice() else {
        panic!("sanitization must preserve the three message boundaries");
    };
    assert_eq!(actual_old, &old);
    assert_eq!(actual_newest, &newest);
    assert_eq!(terminal, &ResponseItemEnvelope::new(compaction_output()));
    assert_eq!(message_text_token_count(&reference.item), reference_tokens);
    let newest_tokens = approx_token_count("new");
    for (budget, expected) in [
        (
            reference_tokens + newest_tokens,
            vec![reference.clone(), newest.clone()],
        ),
        (reference_tokens + newest_tokens - 1, vec![newest.clone()]),
        (newest_tokens, vec![newest]),
        (0, Vec::new()),
    ] {
        assert_eq!(
            truncate_retained_messages_for_remote_compaction(retained[..3].to_vec(), budget),
            expected,
        );
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
    let text = McpServerUseInstructions::new("linear".to_string(), r#"["linear"]"#.to_string())
        .render()
        .expect_text();
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
