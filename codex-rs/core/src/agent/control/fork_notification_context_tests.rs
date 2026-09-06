use super::*;
use codex_history::CodexHarnessMetadata;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::AgentStatus;
use pretty_assertions::assert_eq;

#[test]
fn fork_removes_trusted_reply_routes_but_preserves_client_authored_copies() {
    let route = ContextualUserFragment::into(AgentReplyRoute::new(
        crate::context::AgentContextIdentity::Canonical {
            agent_id: codex_protocol::ThreadId::new(),
        },
    ));
    let parent = ResponseItemEnvelope::new(route);
    let mut runtime = parent.clone();
    assert!(!retain_without_notification_context(&mut runtime));
    let mut quoted = parent;
    quoted.metadata = Some(CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let expected = quoted.clone();
    assert!(retain_without_notification_context(&mut quoted));
    assert_eq!(quoted, expected);
}

#[test]
fn fork_does_not_inherit_persistent_route_guidance() {
    let item = ContextualUserFragment::into(AgentReplyRoute::until_disabled(
        crate::context::AgentContextIdentity::Canonical {
            agent_id: codex_protocol::ThreadId::new(),
        },
    ));
    let mut envelope = ResponseItemEnvelope::new(item);
    assert!(!retain_without_notification_context(&mut envelope));
}

#[test]
fn notification_filter_requires_runtime_annotation() {
    let notification = ContextualUserFragment::into(SubagentNotification::new(
        crate::context::AgentContextIdentity::V2 {
            agent_id: ThreadId::new(),
            agent_path: "/root/worker".try_into().expect("worker path"),
        },
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
        crate::context::AgentContextIdentity::V2 {
            agent_id: ThreadId::new(),
            agent_path: "/root/worker".try_into().expect("worker path"),
        },
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
    let mut expected = envelope.clone();
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
