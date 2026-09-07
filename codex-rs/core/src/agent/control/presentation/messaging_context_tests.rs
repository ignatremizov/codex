use super::*;
use crate::context::AgentReplyRoute;
use codex_history::CodexHarnessMetadata;
use codex_protocol::ResponseItemId;
use pretty_assertions::assert_eq;

#[test]
fn unchanged_policy_preserves_item_ids_order_and_harness_metadata() {
    let notice = ContextualUserFragment::into(PermissionNotice {
        key: format!("directed.{}", ThreadId::new()),
        text: "User disabled send_input to Pascal (3).".to_string(),
    });
    let mut stored = notice.clone();
    stored.set_id(Some(ResponseItemId::new("msg")));
    let snapshot = MessagingContextSnapshot {
        notices: vec![notice],
        ..Default::default()
    };
    let mut items = vec![ResponseItemEnvelope {
        item: stored,
        metadata: Some(CodexHarnessMetadata {
            client_authored: false,
            fallback_token_limit_override: Some(123),
        }),
    }];
    let expected = items.clone();
    snapshot.reconcile_annotated(&mut items);
    snapshot.reconcile_annotated(&mut items);
    assert_eq!(items, expected);
}

#[test]
fn reconstructed_history_uses_current_disable_and_preserves_singleton_identity_hint() {
    let recipient = ThreadId::new();
    let key = format!("directed.{recipient}");
    let enabled = ContextualUserFragment::into(PermissionNotice {
        key: key.clone(),
        text: "User enabled send_input to Pascal (3).".to_string(),
    });
    let disabled = ContextualUserFragment::into(PermissionNotice {
        key,
        text: "User disabled send_input to Pascal (3).".to_string(),
    });
    let route =
        ContextualUserFragment::into(AgentReplyRoute::until_disabled(AgentContextIdentity::V1 {
            agent_id: recipient,
            agent_ref: Some(3),
            nickname: Some("Pascal".to_string()),
            task_path: None,
        }));
    let original = vec![
        ResponseItemEnvelope::new(enabled),
        ResponseItemEnvelope::new(route),
    ];
    let mut projected = original.clone();
    let snapshot = MessagingContextSnapshot {
        notices: vec![disabled.clone()],
        ..Default::default()
    };
    snapshot.reconcile_annotated(&mut projected);
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[1].item, disabled);
    let ResponseItem::Message { content, .. } = &projected[0].item else {
        panic!("identity hint remains a message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("identity hint remains one text item");
    };
    let body = text
        .strip_prefix("<agent_reply_route>")
        .and_then(|body| body.strip_suffix("</agent_reply_route>"))
        .expect("route envelope");
    assert_eq!(
        serde_json::from_str::<Value>(body).expect("route JSON"),
        serde_json::json!({
            "agent_id": recipient.to_string(),
            "nickname": "Pascal",
            "ref": "3",
            "send_input": "not_authorized",
        }),
    );
    let stable = projected.clone();
    snapshot.reconcile_annotated(&mut projected);
    assert_eq!(projected, stable);
    assert_ne!(
        projected, original,
        "projection does not mutate stored history"
    );
}

#[test]
fn cold_runtime_discards_historical_enable_without_restoring_authority() {
    let notice = ContextualUserFragment::into(PermissionNotice {
        key: format!("subtree.{}", ThreadId::new()),
        text: "User enabled send_input within Main's subtree.".to_string(),
    });
    let mut items = vec![notice];
    let snapshot = MessagingContextSnapshot::default();
    snapshot.reconcile(&mut items);
    assert_eq!(
        items.iter().filter_map(notice_parts).collect::<Vec<_>>(),
        vec![("baseline", "No live user messaging overrides are active.")],
    );
    let first = items.clone();
    snapshot.reconcile(&mut items);
    assert_eq!(items, first);
}
