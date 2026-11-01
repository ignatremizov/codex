use super::*;
use crate::context::world_state::WorldState;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn receiver(thread_id: ThreadId, agent_ref: Option<u64>) -> AgentReceiverIdentityState {
    let mut snapshot = V1AgentIdentitySnapshot::default();
    snapshot.receiver_thread_id = Some(thread_id);
    snapshot.receiver_ref = agent_ref;
    AgentReceiverIdentityState::new(&snapshot).expect("receiver identity")
}

fn payload(item: &ResponseItemEnvelope) -> Value {
    let ResponseItem::Message { role, content, .. } = &item.item else {
        panic!("identity message");
    };
    assert_eq!(role, "developer");
    let ContentItem::InputText { text } = &content[0] else {
        panic!("identity text");
    };
    let body = text
        .strip_prefix("<agent_self>\n")
        .and_then(|text| text.strip_suffix("\n</agent_self>"))
        .expect("receiver framing");
    serde_json::from_str(body.split_once('\n').expect("identity instructions").1)
        .expect("receiver JSON")
}

#[test]
fn receiver_identity_supersedes_inherited_parent_and_ignores_user_lookalikes() {
    let root = ThreadId::new();
    let child = ThreadId::new();
    let parent = receiver(root, Some(1));
    let current = receiver(child, Some(36));
    let inherited = ResponseItemEnvelope::new(ContextualUserFragment::into(parent));
    let mut user_lookalike =
        ResponseItemEnvelope::new(ContextualUserFragment::into(current.clone()));
    if let ResponseItem::Message { role, .. } = &mut user_lookalike.item {
        *role = "user".into();
    }
    let mut items = vec![inherited, user_lookalike];
    let prefix = items.clone();
    current.reconcile_annotated(&mut items);
    assert_eq!(&items[..prefix.len()], prefix.as_slice());
    assert_eq!(
        payload(items.last().expect("current receiver")),
        json!({"agent_id": child, "self_ref": "36"}),
    );
    current.reconcile_annotated(&mut items);
    assert_eq!(items.len(), 3);
}

#[test]
fn promotion_adoption_and_uuid_fallback_replace_the_same_receivers_previous_ref() {
    let thread_id = ThreadId::new();
    let mut items = Vec::new();
    for agent_ref in [Some(36), Some(1), Some(42), None] {
        receiver(thread_id, agent_ref).reconcile_annotated(&mut items);
        assert_eq!(
            payload(items.last().expect("current receiver")),
            json!({
                "agent_id": thread_id,
                "self_ref": agent_ref.map(|agent_ref| agent_ref.to_string()),
            }),
        );
    }
    assert_eq!(items.len(), 4);
}

#[test]
fn missing_identity_is_restored_even_with_a_retained_world_state_checkpoint() {
    let current = receiver(ThreadId::new(), Some(36));
    let mut world = WorldState::default();
    world.add_section(current.clone());
    let (snapshot, fragments) = world.render_full_fragments();
    let retained = fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let (unchanged, updates) = world.render_history_fragment_diff(Some(&snapshot), retained.iter());
    assert_eq!(unchanged, snapshot);
    assert!(updates.is_empty());
    let (restored, updates) = world.render_history_fragment_diff(Some(&snapshot), &[]);
    assert_eq!(restored, snapshot);
    assert_eq!(
        updates
            .into_iter()
            .map(ContextualUserFragment::into_boxed_response_item)
            .collect::<Vec<_>>(),
        vec![ContextualUserFragment::into(current)],
    );
}
