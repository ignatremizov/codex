use super::*;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use codex_history::CodexHarnessMetadata;
use codex_protocol::models::ContentItemKind;
use pretty_assertions::assert_eq;

#[test]
fn fork_removes_runtime_goal_context_but_preserves_literal_user_and_client_text() {
    let goal = ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("goal"),
        "finish the task",
    ));
    let mut runtime = ResponseItemEnvelope::new(goal.clone());
    assert!(!retain_without_goal_context(&mut runtime));

    for (role, kind, client_authored) in [
        ("user", Some("user.text"), false),
        ("user", None, false),
        ("developer", Some("goal.internal_context"), true),
    ] {
        let mut envelope = ResponseItemEnvelope {
            item: goal.clone(),
            metadata: Some(CodexHarnessMetadata {
                client_authored,
                inherited_user_message: true,
                user_input_order: Some(5),
                ..Default::default()
            }),
        };
        if let ResponseItem::Message {
            role: item_role,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut envelope.item
        {
            *item_role = role.to_string();
            internal_chat_message_metadata_passthrough
                .as_mut()
                .expect("metadata")
                .content_item_kinds = kind.map(|kind| vec![ContentItemKind(kind.to_string())]);
        }
        let expected = envelope.clone();
        assert!(retain_without_goal_context(&mut envelope));
        assert_eq!(envelope, expected);
    }
}
