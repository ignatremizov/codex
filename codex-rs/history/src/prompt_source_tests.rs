use super::UserPromptSource;
use crate::CodexHarnessMetadata;
use crate::RetainedSource;
use crate::RetainedSourceId;
use crate::RetainedSourceRole;
use crate::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn response(text: &str) -> RolloutItem {
    serde_json::from_value(serde_json::json!({
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        },
    }))
    .expect("user response")
}

#[test]
fn repeated_inputs_pair_by_source_order_in_either_persistence_order() {
    for (inputs, presentations) in [([1, 3, 5], [2, 4, 6]), ([2, 4, 6], [1, 3, 5])] {
        let mut source = UserPromptSource::default();
        for position in inputs {
            source.observe(&response("same input"), position);
        }
        assert_eq!(source.boundary(&presentations, /*selected*/ 2), Some(5));
    }
}

#[test]
fn hidden_context_is_excluded_and_incomplete_or_crossed_pairs_are_rejected() {
    let mut context = response("hidden context");
    let RolloutItem::ResponseItem(envelope) = &mut context else {
        unreachable!()
    };
    let codex_protocol::models::ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut envelope.item
    else {
        unreachable!()
    };
    internal_chat_message_metadata_passthrough
        .get_or_insert_default()
        .content_item_kinds = Some(vec![codex_protocol::models::ContentItemKind(
        "harness.context".into(),
    )]);
    let mut source = UserPromptSource::default();
    source.observe(&response("first prompt"), /*position*/ 1);
    source.observe(&context, /*position*/ 3);
    source.observe(&response("second prompt"), /*position*/ 4);
    assert_eq!(source.boundary(&[2, 5], /*selected*/ 1), Some(4));
    assert_eq!(source.boundary(&[2, 5, 6], /*selected*/ 1), None);
    assert_eq!(source.boundary(&[5, 6], /*selected*/ 1), None);
}

#[test]
fn goal_updates_do_not_consume_prompt_pairs_even_with_retained_user_provenance() {
    for kind in ["user.goal", "user.goal.omitted"] {
        for retained_source in [false, true] {
            let mut goal = response("User set the goal: preserve the earlier instructions");
            let RolloutItem::ResponseItem(envelope) = &mut goal else {
                unreachable!()
            };
            let ResponseItem::Message {
                internal_chat_message_metadata_passthrough,
                ..
            } = &mut envelope.item
            else {
                unreachable!()
            };
            internal_chat_message_metadata_passthrough
                .get_or_insert_default()
                .content_item_kinds = Some(vec![ContentItemKind(kind.into())]);
            if retained_source {
                envelope.metadata = Some(CodexHarnessMetadata {
                    retained_source: Some(RetainedSource {
                        id: RetainedSourceId {
                            message_id: "goal".into(),
                            turn_id: "turn".into(),
                            role: RetainedSourceRole::User,
                        },
                        revision: ResponseItemId::with_suffix("revision", "goal"),
                        complete: true,
                    }),
                    ..Default::default()
                });
            }
            let mut source = UserPromptSource::default();
            source.observe(&goal, /*position*/ 0);
            // A goal alone must not invalidate a presentation-only legacy history.
            assert_eq!(source.boundary(&[1], /*selected*/ 0), Some(1));
            // Identical wrapper text without host metadata is still an ordinary user prompt.
            source.observe(
                &response("<codex_internal_context source=\"user_goal\">copy this</codex_internal_context>"),
                /*position*/ 1,
            );
            source.observe(&goal, /*position*/ 3);
            source.observe(&response("steer"), /*position*/ 4);
            assert_eq!(source.boundary(&[2, 5], /*selected*/ 1), Some(4));
        }
    }
}
