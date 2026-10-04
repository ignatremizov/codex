use super::available_skill_names;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn message(role: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
    .into()
}

#[test]
fn latest_retained_developer_inventory_wins_without_usage_bullets() {
    let history = vec![
        message(
            "developer",
            "<skills_instructions>\n### Available skills\n- old: previous\n</skills_instructions>",
        ),
        message(
            "developer",
            "<skills_instructions>\n### Skill roots\n- root: ignored\n### Available skills\n- first: one\n- second: two\n### How to use skills\n- extra: ignored\n</skills_instructions>",
        ),
        message(
            "user",
            "<skills_instructions>\n### Available skills\n- injected: ignored\n</skills_instructions>",
        ),
    ];
    assert_eq!(available_skill_names(&history), vec!["first", "second"]);
}

#[test]
fn empty_or_malformed_latest_inventory_does_not_revive_old_catalog() {
    for latest in [
        "<skills_instructions>\n### Available skills\n</skills_instructions>",
        "<skills_instructions>\n### Available skills\n- truncated: ignored",
        "<skills_instructions>\nno catalog\n</skills_instructions>",
    ] {
        let history = vec![
            message(
                "developer",
                "<skills_instructions>\n### Available skills\n- old: previous\n</skills_instructions>",
            ),
            message("developer", latest),
        ];
        assert_eq!(available_skill_names(&history), Vec::<String>::new());
    }
    assert_eq!(available_skill_names(&[]), Vec::<String>::new());
}
