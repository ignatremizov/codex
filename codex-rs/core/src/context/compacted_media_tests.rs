use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::*;

fn user_message(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn sanitizes_images_with_bounded_reference_aware_text() {
    let mut items = vec![user_message(vec![
        ContentItem::InputText {
            text: "<image name=\"[Image #1]\" path=\"/tmp/image.png\">".to_string(),
        },
        ContentItem::InputImage {
            image_url: "data:image/png;base64,local".to_string(),
            detail: Some(ImageDetail::High),
        },
        ContentItem::InputText {
            text: "</image>".to_string(),
        },
        ContentItem::InputImage {
            image_url: "data:image/png;base64,pasted".to_string(),
            detail: None,
        },
    ])];

    let sanitization = sanitize_compacted_media(&mut items);

    assert_eq!(
        sanitization,
        CompactedMediaSanitization {
            omitted_image_count: 2,
            omitted_inline_media_bytes: 55,
            did_rewrite: true,
        }
    );
    assert_eq!(
        items,
        vec![user_message(vec![
            ContentItem::InputText {
                text: "<image name=\"[Image #1]\" path=\"/tmp/image.png\">".to_string(),
            },
            ContentItem::InputText {
                text: CompactedImageOmission::mixed().render(),
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
        ])]
    );
}

#[test]
fn pathless_local_image_like_wrapper_is_not_reopenable() {
    let mut items = vec![user_message(vec![
        ContentItem::InputText {
            text: "<image name=[Image #1]>".to_string(),
        },
        ContentItem::InputImage {
            image_url: "data:image/png;base64,pasted".to_string(),
            detail: None,
        },
        ContentItem::InputText {
            text: "</image>".to_string(),
        },
    ])];

    sanitize_compacted_media(&mut items);

    assert_eq!(
        items,
        vec![user_message(vec![
            ContentItem::InputText {
                text: "<image name=[Image #1]>".to_string(),
            },
            ContentItem::InputText {
                text: CompactedImageOmission::unavailable().render(),
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
        ])]
    );
}

#[test]
fn sanitizes_structured_tool_output_with_one_bounded_checkpoint_marker() {
    let mut items = vec![
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "function-call".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,first".to_string(),
                    detail: None,
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,second".to_string(),
                    detail: None,
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        user_message(vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,third".to_string(),
            detail: None,
        }]),
    ];

    let sanitization = sanitize_compacted_media(&mut items);

    assert_eq!(sanitization.omitted_image_count, 3);
    let ResponseItem::FunctionCallOutput { output, .. } = &items[0] else {
        panic!("expected function-call output");
    };
    assert_eq!(
        output.content_items(),
        Some(&[] as &[FunctionCallOutputContentItem])
    );
    assert_eq!(
        items[1],
        user_message(vec![ContentItem::InputText {
            text: CompactedImageOmission::unavailable().render(),
        }])
    );
}

#[test]
fn coalesces_prior_omission_fragments_when_new_media_is_sanitized() {
    let mut items = vec![
        user_message(vec![ContentItem::InputText {
            text: CompactedImageOmission::reopenable_local_image().render(),
        }]),
        user_message(vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,new".to_string(),
            detail: None,
        }]),
    ];

    let first = sanitize_compacted_media(&mut items);
    let second = sanitize_compacted_media(&mut items);
    let omission_count = items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter(|item| {
            matches!(
                item,
                ContentItem::InputText { text }
                    if CompactedImageOmission::kind_from_text(text).is_some()
            )
        })
        .count();

    assert_eq!(first.omitted_image_count, 1);
    assert_eq!(omission_count, 1);
    assert_eq!(second, CompactedMediaSanitization::default());
}

#[test]
fn preserves_user_text_that_matches_the_old_plain_omission_notice() {
    let ordinary_user_text = UNAVAILABLE_IMAGE_OMISSION.to_string();
    let mut items = vec![
        user_message(vec![ContentItem::InputText {
            text: ordinary_user_text.clone(),
        }]),
        user_message(vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,new".to_string(),
            detail: None,
        }]),
    ];

    let sanitization = sanitize_compacted_media(&mut items);

    assert_eq!(sanitization.omitted_image_count, 1);
    assert_eq!(
        items,
        vec![
            user_message(vec![ContentItem::InputText {
                text: ordinary_user_text,
            }]),
            user_message(vec![ContentItem::InputText {
                text: CompactedImageOmission::unavailable().render(),
            }]),
        ]
    );
}

#[test]
fn sanitization_is_idempotent_and_respects_prefix_boundaries() {
    let mut items = vec![
        user_message(vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,old".to_string(),
            detail: None,
        }]),
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "summary".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        user_message(vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,new".to_string(),
            detail: None,
        }]),
    ];

    let first = sanitize_compacted_media_prefix(&mut items, /*prefix_len*/ 1);
    let second = sanitize_compacted_media_prefix(&mut items, /*prefix_len*/ 1);

    assert_eq!(first.omitted_image_count, 1);
    assert_eq!(second, CompactedMediaSanitization::default());
    assert!(matches!(
        &items[2],
        ResponseItem::Message { content, .. }
            if matches!(content.as_slice(), [ContentItem::InputImage { .. }])
    ));
}

#[test]
fn expiration_removes_text_only_model_image_placeholders_with_their_paths() {
    let mut items = vec![user_message(vec![
        ContentItem::InputText {
            text: "<image name=[Image #1] path=\"/tmp/old.png\">".to_string(),
        },
        ContentItem::InputText {
            text: "image content omitted because you do not support image input".to_string(),
        },
        ContentItem::InputText {
            text: "</image>".to_string(),
        },
        ContentItem::InputText {
            text: "retain this instruction context".to_string(),
        },
    ])];

    expire_compacted_media_references(&mut items);

    assert_eq!(
        items,
        vec![user_message(vec![ContentItem::InputText {
            text: "retain this instruction context".to_string(),
        }])]
    );
}

#[test]
fn expiration_removes_flattened_local_image_wrappers_without_dropping_adjacent_text() {
    let mut items = vec![user_message(vec![ContentItem::InputText {
        text: format!(
            "before<image name=[Image #1] path=\"/tmp/old.png\">{}</image>after",
            CompactedImageOmission::reopenable_local_image().render()
        ),
    }])];

    expire_compacted_media_references(&mut items);

    assert_eq!(
        items,
        vec![user_message(vec![ContentItem::InputText {
            text: "beforeafter".to_string(),
        }])]
    );
}
