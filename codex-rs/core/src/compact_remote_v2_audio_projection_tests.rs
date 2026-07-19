//! Rejection projection preserves canonical envelopes and the inherited-prefix boundary.

use super::project_replacement_file_audio;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::AudioReference;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::input_text;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

fn envelope(content: Vec<ContentItem>, sequence: u64) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content,
            status: None,
            phase: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            history_sequence: Some(sequence),
            user_input_order: Some(sequence),
            ..Default::default()
        }),
    }
}

fn audio(id: &str) -> ContentItem {
    ContentItem::InputAudio {
        audio: AudioReference::File {
            file_id: id.to_string(),
        },
    }
}

#[test]
fn rejected_audio_removes_rows_without_moving_current_images_into_the_compacted_prefix() {
    let inherited = envelope(
        vec![input_text("retained inherited text"), audio("expired")],
        2,
    );
    let current = envelope(
        vec![
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,current".to_string(),
                },
                detail: None,
            },
            audio("current-audio"),
        ],
        3,
    );
    let following = envelope(vec![input_text("current follow-up")], 5);
    let mut input = vec![
        envelope(vec![audio("expired")], 1),
        inherited.clone(),
        current.clone(),
        envelope(vec![audio("expired")], 4),
        following.clone(),
    ];
    let mut prefix_len = 2;
    project_replacement_file_audio(&mut input, &mut prefix_len, &BTreeSet::new());
    assert_eq!(input.len(), 5);
    assert_eq!(prefix_len, 2);
    project_replacement_file_audio(
        &mut input,
        &mut prefix_len,
        &BTreeSet::from(["expired".to_string()]),
    );
    let mut expected_inherited = inherited;
    let ResponseItem::Message { content, .. } = &mut expected_inherited.item else {
        unreachable!();
    };
    content.pop();
    assert_eq!(prefix_len, 1);
    assert_eq!(input, vec![expected_inherited, current, following]);
}
