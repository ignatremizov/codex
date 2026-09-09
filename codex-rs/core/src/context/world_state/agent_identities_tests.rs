use super::*;
use crate::context::world_state::WorldState;
use pretty_assertions::assert_eq;

#[test]
fn hydration_is_restored_after_compaction_and_unchanged_state_is_not_repeated() {
    let current = AgentIdentitiesState {
        body: "current mapping".to_string(),
    };
    let mut world = WorldState::default();
    world.add_section(current.clone());
    assert!(world.render_diff(&world.snapshot()).is_empty());
    assert_eq!(
        world
            .render_history_diff(Some(&world.snapshot()), &[])
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec![current.render()],
    );
}

#[test]
fn reconcile_keeps_one_current_mapping_and_preserves_other_content_and_annotations() {
    let previous = AgentIdentitiesState {
        body: "old mapping".to_string(),
    };
    let current = AgentIdentitiesState {
        body: "new mapping".to_string(),
    };
    let mut combined = ContextualUserFragment::into(previous);
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut combined
    else {
        panic!("developer message");
    };
    content.push(ContentItem::InputText {
        text: "unrelated developer instructions".to_string(),
    });
    internal_chat_message_metadata_passthrough
        .as_mut()
        .unwrap()
        .content_item_kinds
        .as_mut()
        .unwrap()
        .push(ContentItemKind("test.other".to_string()));
    let mut retained = ResponseItemEnvelope::new(ContextualUserFragment::into(current.clone()));
    retained.metadata = Some(codex_history::CodexHarnessMetadata {
        client_authored: false,
        fallback_token_limit_override: Some(321),
    });
    let mut items = vec![
        ResponseItemEnvelope::new(combined.clone()),
        retained.clone(),
        retained.clone(),
    ];
    current.reconcile_annotated(&mut items);
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut combined
    else {
        panic!("developer message");
    };
    content.remove(0);
    internal_chat_message_metadata_passthrough
        .as_mut()
        .unwrap()
        .content_item_kinds
        .as_mut()
        .unwrap()
        .remove(0);
    let expected = vec![ResponseItemEnvelope::new(combined), retained];
    assert_eq!(items, expected);
    current.reconcile_annotated(&mut items);
    assert_eq!(items, expected);
    items.clear();
    current.reconcile_annotated(&mut items);
    assert_eq!(
        items,
        vec![ResponseItemEnvelope::new(ContextualUserFragment::into(
            current
        ))],
    );
    AgentIdentitiesState::new(&V1AgentIdentitySnapshot::default()).reconcile_annotated(&mut items);
    assert!(
        items.is_empty(),
        "missing authority must not retain a stale mapping"
    );
}
