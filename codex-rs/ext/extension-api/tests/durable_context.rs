use codex_extension_api::ActiveGoalObjective;
use codex_extension_api::RestoredSkillsInventory;
use codex_extension_api::TurnInputContribution;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[test]
fn stale_projection_cannot_survive_disable_or_replace_current_generation() {
    let projection = ActiveGoalObjective::default();
    let first = projection.set_enabled(true).expect("enable");
    assert!(projection.project_if_current(first, "first".to_string()));
    let second = projection.advance_generation().expect("advance");
    assert_eq!(projection.snapshot(), Some("first".to_string()));
    assert!(projection.project_if_current(second, "second".to_string()));
    assert!(!projection.clear_if_current(first));
    assert_eq!(projection.snapshot(), Some("second".to_string()));
    projection.set_enabled(false).expect("disable");
    assert!(!projection.project_if_current(second, "stale".to_string()));
    assert_eq!(projection.snapshot(), None);
    let third = projection.set_enabled(true).expect("re-enable");
    assert!(!projection.project_if_current(second, "stale".to_string()));
    assert!(projection.project_if_current(third, "third".to_string()));
}

#[test]
fn dropped_acknowledgement_does_not_commit_extension_state() {
    let count = Arc::new(AtomicUsize::new(0));
    for commit in [false, true] {
        let observed = Arc::clone(&count);
        let (_, acknowledgement) =
            TurnInputContribution::with_acknowledgement(Vec::new(), move || {
                observed.fetch_add(1, Ordering::SeqCst);
            })
            .into_parts();
        if commit {
            acknowledgement.expect("ack").acknowledge();
        }
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

fn inventory(metadata: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: Some(ResponseItemId::from_server("catalog-source".to_string())),
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: format!(
                    "<skills_instructions>\n<promoted_skills>{metadata}</promoted_skills>\n## Skills\n</skills_instructions>"
                ),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind("skills.catalog".to_string())]),
                    ..Default::default()
                },
            ),
        },
        metadata: Some(CodexHarnessMetadata {
            user_input_order: Some(17),
            ..Default::default()
        }),
    }
}

#[test]
fn inventory_restoration_preserves_full_envelope_and_latest_explicit_empty() {
    let promoted =
        inventory(r#"[{"authorityKindHex":"686f7374","authorityIdHex":"61","packageHex":"62"}]"#);
    let empty = inventory("[]");
    let malformed = inventory("[{}]");
    let history = [promoted, empty.clone(), malformed];
    let selected = history
        .iter()
        .rev()
        .find_map(RestoredSkillsInventory::from_envelope)
        .expect("latest complete inventory");
    assert_eq!(selected.envelope(), &empty);
}

#[test]
fn inventory_restoration_rejects_wrong_role_provenance_shape_and_truncated_wrapper() {
    let canonical = inventory("[]");
    let mut client = canonical.clone();
    client.metadata.as_mut().expect("metadata").client_authored = true;
    let mut user = canonical.clone();
    if let ResponseItem::Message { role, .. } = &mut user.item {
        *role = "user".to_string();
    }
    let mut unknown = canonical.clone();
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut unknown.item
    {
        *internal_chat_message_metadata_passthrough = None;
    }
    let mut incomplete = canonical;
    if let ResponseItem::Message { content, .. } = &mut incomplete.item {
        let ContentItem::InputText { text } = &mut content[0] else {
            unreachable!();
        };
        *text = text.replace("</skills_instructions>", "");
    }
    for rejected in [
        client,
        user,
        unknown,
        incomplete,
        inventory("[{}]"),
        inventory(r#"[{"authorityKindHex":"zz","authorityIdHex":"61","packageHex":"62"}]"#),
    ] {
        assert!(RestoredSkillsInventory::from_envelope(&rejected).is_none());
    }
}
