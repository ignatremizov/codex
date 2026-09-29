use super::ContextManager;
use codex_attachment_store::InlineAttachmentStore;
use codex_context_fragments::AnsweredQuestion;
use codex_context_fragments::ContextualUserFragment;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol_test_support::ContentItemTestExt;
use pretty_assertions::assert_eq;

#[test_case::test_case(false; "text_only")]
#[test_case::test_case(true; "with_rejected_audio")]
#[tokio::test]
async fn question_projection_changes_only_the_model_copy_and_preserves_source_metadata(
    audio: bool,
) {
    let identity = r#"["request_user_input_async","async-question:q7:source-call",1]"#;
    let reply = AnsweredQuestion::new(identity, "Which environment?", "Staging")
        .render()
        .expect_text();
    let mut canonical = ResponseItemEnvelope {
        item: ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: Some(ResponseItemId::new("canonical-user-reply")),
            role: "user".into(),
            content: vec![ContentItem::InputText { text: reply }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind(
                        "user.answered_question".into(),
                    )]),
                    ..Default::default()
                },
            ),
        },
        metadata: Some(CodexHarnessMetadata::default()),
    };
    if audio {
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut canonical.item
        else {
            unreachable!()
        };
        content.push(ContentItem::InputAudio {
            audio: codex_protocol::models::AudioReference::File {
                file_id: "rejected-audio".into(),
            },
        });
        internal_chat_message_metadata_passthrough
            .as_mut()
            .expect("content classification")
            .content_item_kinds
            .as_mut()
            .expect("content kinds")
            .push(ContentItemKind("unknown".into()));
    }
    let mut history = ContextManager::default();
    history.replace_annotated(vec![canonical.clone()]);
    canonical.metadata = Some(CodexHarnessMetadata {
        history_sequence: Some(1),
        sampled_through_sequence: Some(0),
        ..Default::default()
    });
    history.restore_rejected_audio_file_ids(std::collections::BTreeSet::from([
        "rejected-audio".to_string()
    ]));
    let mut projected = canonical.clone();
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut projected.item
    else {
        unreachable!("test builds a message");
    };
    *content = vec![ContentItem::InputText {
        text: "q8: Staging".into(),
    }];
    internal_chat_message_metadata_passthrough
        .as_mut()
        .expect("content classification")
        .content_item_kinds
        .as_mut()
        .expect("content kinds")
        .truncate(1);
    assert_eq!(
        history
            .clone()
            .for_prompt_annotated(
                &[InputModality::Text, InputModality::Audio],
                &InlineAttachmentStore
            )
            .await,
        vec![projected]
    );
    assert_eq!(history.annotated_items(), std::slice::from_ref(&canonical));
}

#[tokio::test]
async fn mixed_text_and_non_user_question_envelopes_are_not_shortened() {
    let identity = r#"["request_user_input_async","async-question:q7:source-call",0]"#;
    let reply = AnsweredQuestion::new(identity, "Which environment?", "Staging")
        .render()
        .expect_text();
    for (role, text) in [
        ("assistant", reply.clone()),
        ("user", format!("{reply}\nAlso keep this instruction.")),
    ] {
        let original = ResponseItemEnvelope {
            item: ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: role.into(),
                content: vec![ContentItem::InputText { text }],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        // Ordinary prompt normalization labels unclassified content unknown.
                        // Start at that boundary so this test isolates question projection.
                        content_item_kinds: Some(vec![ContentItemKind("unknown".into())]),
                        ..Default::default()
                    },
                ),
            },
            metadata: Some(CodexHarnessMetadata {
                history_sequence: Some(1),
                sampled_through_sequence: Some(0),
                ..Default::default()
            }),
        };
        let mut history = ContextManager::default();
        history.replace_annotated(vec![original.clone()]);
        assert_eq!(
            history
                .for_prompt_annotated(&[InputModality::Text], &InlineAttachmentStore)
                .await,
            vec![original]
        );
    }
}
