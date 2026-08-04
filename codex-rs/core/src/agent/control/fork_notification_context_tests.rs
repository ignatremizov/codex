use super::*;
use codex_history::CodexHarnessMetadata;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::AgentStatus;
use pretty_assertions::assert_eq;

#[test]
fn notification_filter_requires_runtime_annotation() {
    let notification = ContextualUserFragment::into(SubagentNotification::new(
        "/root/worker",
        codex_protocol::ThreadId::new(),
        AgentStatus::Completed(Some("finished".to_string())),
    ));
    let parent = ResponseItemEnvelope::new(notification);
    let mut child = parent.clone();
    assert!(!retain_without_notification_context(&mut child));

    for (role, kind, client_authored) in [
        ("user", Some("user.text"), false),
        ("user", None, false),
        ("user", Some("multi_agent.subagent_notification"), true),
        (
            "developer",
            Some("multi_agent.subagent_notification"),
            false,
        ),
    ] {
        let mut envelope = parent.clone();
        envelope.metadata = Some(CodexHarnessMetadata {
            client_authored,
            ..Default::default()
        });
        if let ResponseItem::Message {
            role: item_role,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut envelope.item
        {
            *item_role = role.to_string();
            internal_chat_message_metadata_passthrough
                .as_mut()
                .expect("notification metadata")
                .content_item_kinds = kind.map(|kind| vec![ContentItemKind(kind.to_string())]);
        }
        let expected = envelope.clone();
        assert!(retain_without_notification_context(&mut envelope));
        assert_eq!(envelope, expected);
    }
}

#[test]
fn notification_filter_preserves_other_fragments_and_annotations() {
    let notification = ContextualUserFragment::into(SubagentNotification::new(
        "/root/worker",
        codex_protocol::ThreadId::new(),
        AgentStatus::Completed(Some("finished".to_string())),
    ));
    let mut envelope = ResponseItemEnvelope::new(notification);
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut envelope.item
    else {
        panic!("notification is a message");
    };
    content.push(ContentItem::InputText {
        text: "literal user instruction".to_string(),
    });
    internal_chat_message_metadata_passthrough
        .as_mut()
        .expect("notification metadata")
        .content_item_kinds
        .as_mut()
        .expect("notification kinds")
        .push(ContentItemKind("user.text".to_string()));
    let source = codex_history::RetainedSource {
        id: codex_history::RetainedSourceId {
            message_id: "mixed-message".to_string(),
            turn_id: "parent-turn".to_string(),
            role: codex_history::RetainedSourceRole::User,
        },
        revision: codex_protocol::ResponseItemId::with_suffix("msg", "mixed-evidence"),
        complete: true,
    };
    envelope.metadata = Some(CodexHarnessMetadata {
        guardian_sources: vec![source.clone()],
        retained_source: Some(source),
        guardian_source_order_guidance: true,
        user_input_order: Some(7),
        ..Default::default()
    });
    let mut expected = envelope.clone();
    expected
        .metadata
        .as_mut()
        .expect("source metadata")
        .mark_retained_sources_incomplete();
    if let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut expected.item
    {
        content.remove(0);
        internal_chat_message_metadata_passthrough
            .as_mut()
            .expect("metadata")
            .content_item_kinds = Some(vec![ContentItemKind("user.text".to_string())]);
    }
    assert!(retain_without_notification_context(&mut envelope));
    assert_eq!(envelope, expected);
}
