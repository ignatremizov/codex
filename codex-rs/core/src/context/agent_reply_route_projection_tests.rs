use super::project_v1_agent_envelopes;
use crate::context::AgentContextIdentity;
use crate::context::AgentReplyRoute;
use crate::context::ContextualUserFragment;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ContentItemMetadata;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol_test_support::ContentItemTestExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;

#[test]
fn reply_routes_are_omitted_without_rewriting_canonical_evidence() {
    let target = ThreadId::new();
    let identity = AgentContextIdentity::V1 {
        agent_id: target,
        agent_ref: Some(99),
        nickname: Some("old nickname".into()),
        task_path: Some("/root/old-task".into()),
    };
    for route in [
        AgentReplyRoute::new(identity.clone()),
        AgentReplyRoute::until_disabled(identity),
    ] {
        let mut canonical = ContextualUserFragment::into(route);
        canonical.set_id(Some(ResponseItemId::new("route")));
        let ResponseItem::Message { content, .. } = &canonical else {
            panic!("route message");
        };
        let ContentItem::InputText { text } = &content[0] else {
            panic!("route text");
        };
        let canonical_text = text.clone();
        for state in [
            "allowed_this_turn",
            "allowed_until_disabled",
            "not_authorized",
        ] {
            let mut raw = canonical.clone();
            let ResponseItem::Message { content, .. } = &mut raw else {
                panic!("route message");
            };
            let fields = json!({
                "agent_id": target,
                "ref": "99",
                "nickname": "old nickname",
                "task_path": "/root/old-task",
                "send_input": state,
            });
            content[0] = ContentItem::InputText {
                text: format!("<agent_reply_route>\n{fields}\n</agent_reply_route>"),
            };
            for aliases in [
                HashMap::from([(target, 2)]),
                HashMap::new(),
                HashMap::from([(ThreadId::new(), 99)]),
            ] {
                let mut projected = vec![raw.clone()];
                project_v1_agent_envelopes(&mut projected, &aliases);
                assert_eq!(projected, Vec::<ResponseItem>::new());
                project_v1_agent_envelopes(&mut projected, &aliases);
                assert_eq!(projected, Vec::<ResponseItem>::new());
            }
        }
        let ResponseItem::Message { content, .. } = canonical else {
            panic!("route message");
        };
        assert_eq!(
            content,
            vec![ContentItem::InputText {
                text: canonical_text
            }]
        );
    }
}

#[test]
fn route_filtering_preserves_ordinary_text_media_and_classification_alignment() {
    let target = ThreadId::new();
    let route =
        AgentReplyRoute::until_disabled(AgentContextIdentity::Canonical { agent_id: target });
    let mut tagged = ContextualUserFragment::into(route.clone());
    tagged.set_id(Some(ResponseItemId::new("mixed-message")));
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &mut tagged
    else {
        panic!("route message");
    };
    // Only the first text has route metadata; marker-looking ordinary text is untouched.
    content.push(ContentItem::InputText {
        text: route.render().expect_text(),
    });
    content.push(ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "data:image/png;base64,aW1hZ2U=".into(),
        },
        detail: None,
    });
    metadata
        .content_item_metadata
        .as_mut()
        .expect("route sources")
        .extend([ContentItemMetadata::user(), ContentItemMetadata::user()]);
    let mut expected = tagged.clone();
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &mut expected
    else {
        panic!("route message");
    };
    content.remove(/*index*/ 0);
    metadata.content_item_kinds = Some(vec![ContentItemKind("unknown".into()); 2]);
    metadata.content_item_metadata = Some(vec![ContentItemMetadata::user(); 2]);
    let mut items = vec![tagged];
    project_v1_agent_envelopes(&mut items, &HashMap::from([(target, 2)]));
    assert_eq!(items, vec![expected.clone()]);
    project_v1_agent_envelopes(&mut items, &HashMap::from([(target, 2)]));
    assert_eq!(items, vec![expected]);

    let mut untagged = ContextualUserFragment::into(route.clone());
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut untagged
    else {
        panic!("route message");
    };
    *internal_chat_message_metadata_passthrough = None;
    let mut developer = ContextualUserFragment::into(route.clone());
    let ResponseItem::Message { role, .. } = &mut developer else {
        panic!("route message");
    };
    *role = "developer".into();
    let mut items = vec![untagged, developer];
    let original = items.clone();
    for body in [
        "null".to_string(),
        "{invalid".to_string(),
        json!({"ref":"2", "send_input":"allowed_this_turn"}).to_string(),
        json!({"agent_id":target, "send_input":"future_scope"}).to_string(),
    ] {
        let mut item = ContextualUserFragment::into(route.clone());
        let ResponseItem::Message { content, .. } = &mut item else {
            panic!("route message");
        };
        content[0] = ContentItem::InputText {
            text: format!("<agent_reply_route>\n{body}\n</agent_reply_route>"),
        };
        items.push(item);
    }
    project_v1_agent_envelopes(&mut items, &HashMap::from([(target, 2)]));
    assert_eq!(items, original);
}

#[test]
fn permission_announcements_are_hidden_without_removing_neighboring_instructions() {
    for text in [
        "User enabled send_input to peer.",
        "User disabled send_input to peer.",
        "No live user messaging overrides are active.",
    ] {
        let canonical: ResponseItem = serde_json::from_value(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": text}],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": ["multi_agent.messaging_policy.baseline"]
            }
        }))
        .expect("historical messaging policy fixture");
        let mut mixed = canonical.clone();
        let ResponseItem::Message { content, .. } = &mut mixed else {
            panic!("policy message");
        };
        content.push(ContentItem::InputText {
            text: "Retain the actual developer instructions.".into(),
        });
        let mut expected = mixed.clone();
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = &mut expected
        else {
            panic!("policy message");
        };
        content.remove(/*index*/ 0);
        metadata.content_item_kinds = Some(vec![ContentItemKind("unknown".into())]);
        let mut projected = vec![canonical, mixed];
        project_v1_agent_envelopes(&mut projected, &HashMap::new());
        assert_eq!(projected, vec![expected]);
    }
}
