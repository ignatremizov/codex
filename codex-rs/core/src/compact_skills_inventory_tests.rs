use super::*;
use codex_protocol::ResponseItemId;
use pretty_assertions::assert_eq;

fn inventory(id: &str, names: &[&str]) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.to_string())),
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: format!(
                "{SKILLS_INSTRUCTIONS_OPEN_TAG}\n<promoted_skills>[]</promoted_skills>\n\
                 ## Skills\n### Available skills\n{}\n{SKILLS_INSTRUCTIONS_CLOSE_TAG}",
                names
                    .iter()
                    .map(|name| format!("- {name}: description"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn available_skill_names_uses_latest_compacted_inventory() {
    assert_eq!(
        available_skill_names(&[
            inventory("old", &["old-skill"]),
            inventory("current", &["backend-coding", "frontend-coding"]),
        ]),
        vec!["backend-coding".to_string(), "frontend-coding".to_string()]
    );
}
