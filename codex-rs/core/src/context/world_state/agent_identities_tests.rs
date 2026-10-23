use super::*;
use crate::context::world_state::WorldState;
use crate::context::world_state::test_support::FragmentSectionTestExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn state(entries: Value) -> AgentIdentitiesState {
    AgentIdentitiesState {
        body: format!(
            "current identity authority\n{}",
            entries
                .to_string()
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
        ),
    }
}

fn payload(fragment: &dyn ContextualUserFragment) -> Value {
    let rendered = fragment.render();
    let body = body_from_rendered_fragment(&rendered, AgentIdentitiesState::type_markers())
        .expect("identity body");
    let payload = body.split_once('\n').map_or(body, |(_, payload)| payload);
    serde_json::from_str(payload).expect("identity payload")
}

fn identity_item(body: String) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ContextualUserFragment::into(AgentIdentityContextFragment {
        body,
    }))
}

#[test]
fn hydration_is_restored_after_compaction_and_unchanged_state_is_not_repeated() {
    let current = state(json!([{"ref":"1", "state":"active"}]));
    let mut world = WorldState::default();
    world.add_section(current.clone());
    let (snapshot, fragments) = world.render_full_fragments();
    let retained = fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let (unchanged_snapshot, unchanged) =
        world.render_history_fragment_diff(Some(&snapshot), retained.iter());
    assert_eq!(unchanged_snapshot, snapshot);
    assert!(unchanged.is_empty());
    let (restored_snapshot, restored) = world.render_history_fragment_diff(Some(&snapshot), &[]);
    assert_eq!(restored_snapshot, snapshot);
    assert_eq!(
        restored
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec![current.render()],
    );
}

#[test]
fn render_diff_emits_only_added_or_changed_entries_and_removed_refs() {
    let original = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"active"}
    ]));
    let added = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"active"},
        {"ref":"3", "nickname":"Hume", "state":"active"}
    ]));
    let changed = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"closed"}
    ]));
    let removed = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"}
    ]));

    let added_diff = added
        .render_fragment_diff(PreviousSectionState::Known(&original.body))
        .1
        .expect("addition delta");
    assert_eq!(
        payload(added_diff.as_ref()),
        json!({
            "upsert":[{"ref":"3", "nickname":"Hume", "state":"active"}],
        }),
    );
    let changed_diff = changed
        .render_fragment_diff(PreviousSectionState::Known(&original.body))
        .1
        .expect("state delta");
    assert_eq!(
        payload(changed_diff.as_ref()),
        json!({
            "patch":[{"ref":"2", "state":"closed"}],
        }),
    );
    let removed_diff = removed
        .render_fragment_diff(PreviousSectionState::Known(&original.body))
        .1
        .expect("removal delta");
    assert_eq!(
        payload(removed_diff.as_ref()),
        json!({"removed_refs":["2"]}),
    );
    assert!(
        original
            .render_fragment_diff(PreviousSectionState::Known(&original.body))
            .1
            .is_none(),
    );
}

#[test]
fn render_diff_uses_full_authority_without_a_usable_previous_snapshot() {
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"active"}
    ]));
    let malformed = "malformed".to_string();
    let empty = String::new();

    for previous in [
        PreviousSectionState::Absent,
        PreviousSectionState::Unknown,
        PreviousSectionState::Known(&malformed),
        PreviousSectionState::Known(&empty),
    ] {
        let (snapshot, fragment) = current.render_fragment_diff(previous);
        assert_eq!(snapshot, Some(current.body.clone()));
        let fragment = fragment.expect("full authority");
        assert_eq!(
            payload(fragment.as_ref()),
            json!([
                {"ref":"1", "nickname":"Main", "state":"active"},
                {"ref":"2", "nickname":"Curie", "state":"active"}
            ]),
        );
    }
}

#[test]
fn partial_updates_clear_labels_add_complete_entries_and_replay_legacy_upserts() {
    let original = state(json!([
        {"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"},
        {"ref":"2", "nickname":"Curie", "task_path":"/root/old", "state":"active"},
        {"ref":"3", "nickname":"Hume", "task_path":null, "state":"active"}
    ]));
    let legacy = "Agent identity changes: upsert by ref, remove listed refs; all other mappings stay unchanged.\n{\"upsert\":[{\"ref\":\"2\",\"nickname\":\"Curie\",\"task_path\":\"/root/new\",\"state\":\"closed\"}],\"removed_refs\":[]}";
    let previous = state(json!([
        {"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"},
        {"ref":"2", "nickname":"Curie", "task_path":"/root/new", "state":"closed"},
        {"ref":"3", "nickname":"Hume", "task_path":null, "state":"active"}
    ]));
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"},
        {"ref":"2", "nickname":null, "task_path":null, "state":"active"},
        {"ref":"4", "nickname":"New", "task_path":"/root/new", "state":"active"}
    ]));
    let delta = current
        .render_fragment_diff(PreviousSectionState::Known(&previous.body))
        .1
        .expect("partial update");
    assert_eq!(
        payload(delta.as_ref()),
        json!({
            "patch":[{"ref":"2", "nickname":null, "task_path":null, "state":"active"}],
            "upsert":[{"ref":"4", "nickname":"New", "task_path":"/root/new", "state":"active"}],
            "removed_refs":["3"],
        }),
    );
    let rendered = delta.render();
    let body = body_from_rendered_fragment(&rendered, AgentIdentitiesState::type_markers())
        .expect("compact body");
    assert_eq!(
        resolve_retained_chain([original.body.as_str(), legacy, body]),
        IdentityMapping::from_authority_body(&current.body),
    );
    let mut items = vec![
        identity_item(original.body),
        identity_item(legacy.to_string()),
    ];
    let retained = items.clone();
    current.reconcile_annotated(&mut items);
    assert_eq!(&items[..retained.len()], retained.as_slice());
    assert_eq!(items.len(), 3);
    current.reconcile_annotated(&mut items);
    assert_eq!(items.len(), 3);
}

#[test]
fn unusable_partial_updates_reset_to_full_authority_without_rewriting_history() {
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"}
    ]));
    for delta in [
        json!({"patch":[{"ref":"2", "state":"closed"}]}),
        json!({"patch":[{"ref":"1"}]}),
        json!({"patch":[{"ref":"1", "nickname":false}]}),
        json!({"patch":[{"ref":"1", "task_path":42}]}),
        json!({"patch":[{"ref":"1", "state":null}]}),
        json!({"patch":[{"ref":"1", "state":"unknown"}]}),
        json!({"patch":[{"ref":"1", "unknown":"value"}]}),
        json!({"patch":[{"ref":"1", "state":"closed"}, {"ref":"1", "state":"active"}]}),
        json!({"patch":[{"ref":"1", "state":"closed"}], "removed_refs":["1"]}),
        json!({"patch":[{"ref":"1", "state":"closed"}], "upsert":[{"ref":"1"}]}),
        json!({"patch":[{"ref":"0", "state":"closed"}]}),
        json!({"patch":null}),
    ] {
        let mut items = vec![
            identity_item(current.body.clone()),
            identity_item(delta.to_string()),
        ];
        let retained = items.clone();
        current.reconcile_annotated(&mut items);
        assert_eq!(&items[..retained.len()], retained.as_slice());
        assert_eq!(
            items.last().expect("full recovery").item,
            ContextualUserFragment::into(current.clone()),
        );
        assert_eq!(items.len(), 3);
        current.reconcile_annotated(&mut items);
        assert_eq!(items.len(), 3);
    }
    let mut items = vec![identity_item(
        json!({"patch":[{"ref":"1", "state":"closed"}]}).to_string(),
    )];
    current.reconcile_annotated(&mut items);
    assert_eq!(
        items.last().expect("missing baseline recovery").item,
        ContextualUserFragment::into(current),
    );
}

#[test]
fn omission_boundary_changes_use_a_full_authority() {
    let entries = json!([{"ref":"1", "nickname":"Main", "state":"active"}]);
    let previous = AgentIdentitiesState {
        body: format!(
            "current identity authority Additional identities are omitted; use the existing agent directory/discovery for details.\n{entries}"
        ),
    };
    let current = state(entries.clone());

    let fragment = current
        .render_fragment_diff(PreviousSectionState::Known(&previous.body))
        .1
        .expect("full authority");

    assert_eq!(payload(fragment.as_ref()), entries);
}

#[test]
fn reconcile_appends_one_delta_and_preserves_retained_content_and_annotations() {
    let previous = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"active"}
    ]));
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"closed"}
    ]));
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
        .expect("metadata")
        .content_item_kinds
        .as_mut()
        .expect("content kinds")
        .push(ContentItemKind("test.other".to_string()));
    let mut retained = ResponseItemEnvelope::new(combined);
    retained.metadata = Some(codex_history::CodexHarnessMetadata {
        client_authored: false,
        history_truncation_token_limit: Some(321),
        ..Default::default()
    });
    let mut items = vec![retained.clone()];

    current.reconcile_annotated(&mut items);

    assert_eq!(items[0], retained);
    assert_eq!(items.len(), 2);
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &items[1].item
    else {
        panic!("delta message");
    };
    assert_eq!(
        internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref()),
        Some(&vec![ContentItemKind("multi_agent.identities".to_string())]),
    );
    let ContentItem::InputText { text } = &content[0] else {
        panic!("delta text");
    };
    let body = body_from_rendered_fragment(text, AgentIdentitiesState::type_markers())
        .expect("delta body");
    assert_eq!(
        serde_json::from_str::<Value>(body).expect("delta JSON"),
        json!({
            "patch":[{"ref":"2", "state":"closed"}],
        }),
    );
    current.reconcile_annotated(&mut items);
    assert_eq!(items.len(), 2, "reconciliation must be idempotent");
}

#[test]
fn reconcile_recovers_missing_or_malformed_bases_with_one_full_snapshot() {
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"closed"}
    ]));
    let mut items = vec![identity_item(
        "incremental update\n{\"upsert\":[{\"ref\":\"2\",\"nickname\":\"Curie\",\"state\":\"closed\"}],\"removed_refs\":[]}"
            .to_string(),
    )];

    current.reconcile_annotated(&mut items);

    assert_eq!(items.len(), 2);
    let ResponseItem::Message { content, .. } = &items[1].item else {
        panic!("full recovery message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("full recovery text");
    };
    let body = body_from_rendered_fragment(text, AgentIdentitiesState::type_markers())
        .expect("full recovery body");
    assert!(
        serde_json::from_str::<Value>(body.split_once('\n').expect("payload").1)
            .is_ok_and(|payload| payload.is_array()),
    );
    current.reconcile_annotated(&mut items);
    assert_eq!(items.len(), 2);

    let mut malformed = vec![identity_item("instructions\n{\"upsert\":null}".to_string())];
    current.reconcile_annotated(&mut malformed);
    assert_eq!(malformed.len(), 2);

    let oversized = state(json!([{"ref":"9".repeat(20_000)}]));
    let mut malformed = vec![ResponseItemEnvelope::new(ContextualUserFragment::into(
        oversized,
    ))];
    current.reconcile_annotated(&mut malformed);
    assert_eq!(malformed.len(), 2);
    assert_eq!(
        malformed.last().expect("bounded recovery").item,
        ContextualUserFragment::into(current.clone()),
    );
    current.reconcile_annotated(&mut malformed);
    assert_eq!(malformed.len(), 2);
}

#[test]
fn empty_authority_clears_stale_refs_but_stays_absent_for_root_only_history() {
    let previous = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"Curie", "state":"closed"}
    ]));
    let empty = AgentIdentitiesState::new(&V1AgentIdentitySnapshot::default());
    let mut items = vec![ResponseItemEnvelope::new(ContextualUserFragment::into(
        previous,
    ))];

    empty.reconcile_annotated(&mut items);

    assert_eq!(items.len(), 2);
    let ResponseItem::Message { content, .. } = &items[1].item else {
        panic!("clear delta message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("clear delta text");
    };
    let body = body_from_rendered_fragment(text, AgentIdentitiesState::type_markers())
        .expect("clear delta body");
    assert_eq!(
        serde_json::from_str::<Value>(body).expect("clear delta JSON"),
        json!({"removed_refs":["1", "2"]}),
    );

    let mut root_only = Vec::new();
    empty.reconcile_annotated(&mut root_only);
    assert!(root_only.is_empty());
}

#[test]
fn delta_rendering_preserves_json_escaping() {
    let previous = state(json!([{"ref":"1", "nickname":"Main", "state":"active"}]));
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "state":"active"},
        {"ref":"2", "nickname":"<agent>", "state":"active"}
    ]));

    let rendered = current
        .render_fragment_diff(PreviousSectionState::Known(&previous.body))
        .1
        .expect("escaped delta")
        .render();

    assert!(rendered.contains(r#""nickname":"\u003cagent\u003e""#));
    assert!(!rendered.contains("<agent>"));
}

#[test]
fn individually_bounded_retained_deltas_cannot_grow_an_unbounded_mapping() {
    let current_entries = json!([{"ref":"1", "nickname":"Main", "state":"active"}]);
    let current = state(current_entries);
    let mut items = vec![identity_item("initial identity authority\n[]".to_string())];
    for batch in 0..8 {
        let upsert = (0..200)
            .map(|entry| {
                json!({
                    "ref": (batch * 200 + entry + 2).to_string(),
                    "nickname": "retained member",
                })
            })
            .collect::<Vec<_>>();
        let body = format!(
            "identity update\n{}",
            json!({
                "upsert": upsert, "removed_refs": [],
            })
        );
        assert!(body.len() < 16_384);
        items.push(identity_item(body));
    }
    let originals = items.clone();
    current.reconcile_annotated(&mut items);
    assert_eq!(&items[..originals.len()], originals.as_slice());
    assert_eq!(items.len(), originals.len() + 1);
    assert_eq!(
        items.last().expect("full bounded recovery").item,
        ContextualUserFragment::into(current.clone()),
    );
    current.reconcile_annotated(&mut items);
    assert_eq!(items.len(), originals.len() + 1);
}

#[test]
fn individually_bounded_partial_updates_cannot_grow_an_unbounded_mapping() {
    let current = state(json!([
        {"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"}
    ]));
    let baseline = state(Value::Array(
        (1..=4)
            .map(|agent_ref| {
                json!({
                    "ref": agent_ref.to_string(),
                    "nickname": null,
                    "task_path": null,
                    "state": "active",
                })
            })
            .collect(),
    ));
    let mut items = vec![identity_item(baseline.body)];
    for agent_ref in 1..=4 {
        let body = json!({"patch":[{
            "ref":agent_ref.to_string(),
            "nickname":"<".repeat(1_000),
        }]})
        .to_string()
        .replace('<', "\\u003c");
        assert!(body.len() < 16_384);
        items.push(identity_item(body));
    }
    let retained = items.clone();
    current.reconcile_annotated(&mut items);
    assert_eq!(&items[..retained.len()], retained.as_slice());
    assert_eq!(items.len(), retained.len() + 1);
    assert_eq!(
        items.last().expect("bounded full recovery").item,
        ContextualUserFragment::into(current.clone()),
    );
    current.reconcile_annotated(&mut items);
    assert_eq!(items.len(), retained.len() + 1);
}

#[test]
fn oversized_removal_delta_falls_back_to_the_complete_current_authority() {
    let previous = state(Value::Array(
        (0..500_u64)
            .map(|offset| {
                json!({
                    "ref": (1_000_000_000_000_000_000_u64 + offset).to_string(),
                })
            })
            .collect(),
    ));
    let current_entries = json!([{"ref":"1", "nickname":"x".repeat(7_000)}]);
    let current = state(current_entries.clone());
    assert!(previous.body.len() < 16_384);
    assert!(current.render().len() < 8_192);
    let fragment = current
        .render_fragment_diff(PreviousSectionState::Known(&previous.body))
        .1
        .expect("bounded full authority");
    assert_eq!(payload(fragment.as_ref()), current_entries);
    assert!(fragment.render().len() <= 16_384);
}
