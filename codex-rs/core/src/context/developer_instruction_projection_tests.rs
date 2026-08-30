use super::project_developer_instructions;
use crate::context::ContextualUserFragment;
use crate::context::DeveloperInstructions;
use crate::context_manager::updates::build_rendered_message;
use codex_context_fragments::AdditionalContextDeveloperFragment;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(Some("current"); "configured override")]
#[test_case(Some(""); "explicit empty")]
fn only_configuration_fragments_change(instructions: Option<&str>) {
    let unchanged = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "client-developer")),
        role: "developer".to_owned(),
        content: vec![ContentItem::InputText {
            text: "old".to_owned(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let bundle = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "context")),
        role: "developer".to_owned(),
        content: vec![
            ContentItem::InputText {
                text: "old".to_owned(),
            },
            ContentItem::InputText {
                text: "managed policy".to_owned(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("historical-turn".to_owned()),
            content_item_kinds: Some(vec![
                DeveloperInstructions::new("").content_kind(),
                ContentItemKind("managed_config.developer_instructions".to_owned()),
            ]),
            ..Default::default()
        }),
    };
    let mut input = vec![
        unchanged.clone(),
        bundle.clone(),
        ContextualUserFragment::into(DeveloperInstructions::new("duplicate old")),
    ];

    project_developer_instructions(&mut input, instructions);

    let mut expected_bundle = bundle;
    if let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &mut expected_bundle
    {
        match instructions.filter(|text| !text.is_empty()) {
            Some(text) => {
                content[0] = ContentItem::InputText {
                    text: text.to_owned(),
                };
            }
            None => {
                content.remove(/*index*/ 0);
                metadata.content_item_kinds = Some(vec![ContentItemKind(
                    "managed_config.developer_instructions".to_owned(),
                )]);
            }
        }
    }
    assert_eq!(input, vec![unchanged, expected_bundle]);
    let once = input.clone();
    project_developer_instructions(&mut input, instructions);
    assert_eq!(input, once);
}

#[test]
fn a_role_can_add_instructions_to_a_history_without_a_configuration_fragment() {
    let previous = ContextualUserFragment::into(AdditionalContextDeveloperFragment::new(
        "client".to_owned(),
        "retained client instructions".to_owned(),
    ));
    let mut input = vec![previous.clone()];

    project_developer_instructions(&mut input, Some("role instructions"));

    assert_eq!(
        input,
        vec![
            ContextualUserFragment::into(DeveloperInstructions::new("role instructions")),
            previous,
        ]
    );
}

#[test]
fn unchanged_configuration_keeps_the_whole_initial_bundle() {
    let bundle = build_rendered_message(vec![
        DeveloperInstructions::new("current").render_fragment(),
        AdditionalContextDeveloperFragment::new(
            "client".to_owned(),
            "other instructions".to_owned(),
        )
        .render_fragment(),
    ]);
    let mut input = bundle.into_iter().collect::<Vec<_>>();
    let expected = input.clone();

    project_developer_instructions(&mut input, Some("current"));

    assert_eq!(input, expected);

    project_developer_instructions(&mut input, /*instructions*/ None);
    assert_eq!(input, expected);
}
