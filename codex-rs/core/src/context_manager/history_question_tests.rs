use super::ContextManager;
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
use pretty_assertions::assert_eq;

#[test]
fn question_projection_changes_only_the_model_copy_and_preserves_source_metadata() {
    let identity = r#"["request_user_input_async","async-question:q7:source-call",1]"#;
    let reply = AnsweredQuestion::new(identity, "Which environment?", "Staging").render();
    let canonical = ResponseItemEnvelope {
        item: ResponseItem::Message {
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
    let mut history = ContextManager::new();
    history.replace_annotated(vec![canonical.clone()]);
    let mut projected = canonical.clone();
    let ResponseItem::Message { content, .. } = &mut projected.item else {
        unreachable!("test builds a message");
    };
    *content = vec![ContentItem::InputText {
        text: "q8: Staging".into(),
    }];
    assert_eq!(
        history.clone().for_prompt_annotated(&[InputModality::Text]),
        vec![projected]
    );
    assert_eq!(history.annotated_items(), std::slice::from_ref(&canonical));
}

#[test]
fn mixed_text_and_non_user_question_envelopes_are_not_shortened() {
    let identity = r#"["request_user_input_async","async-question:q7:source-call",0]"#;
    let reply = AnsweredQuestion::new(identity, "Which environment?", "Staging").render();
    for (role, text) in [
        ("assistant", reply.clone()),
        ("user", format!("{reply}\nAlso keep this instruction.")),
    ] {
        let original = ResponseItemEnvelope {
            item: ResponseItem::Message {
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
            metadata: Some(CodexHarnessMetadata::default()),
        };
        let mut history = ContextManager::new();
        history.replace_annotated(vec![original.clone()]);
        assert_eq!(
            history.for_prompt_annotated(&[InputModality::Text]),
            vec![original]
        );
    }
}
