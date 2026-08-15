use std::path::PathBuf;

use super::*;
use crate::bottom_pane::LocalImageAttachment;
use codex_app_server_protocol::SkillMetadata;
use codex_utils_absolute_path::test_support::PathBufExt;
use pretty_assertions::assert_eq;

#[test]
fn availability_distinguishes_open_closed_current_and_unknown_targets() {
    let mut navigation = AgentNavigationState::default();
    let current =
        ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad").expect("valid thread id");
    let open =
        ThreadId::from_string("019fbb08-bb4e-79e4-afdb-77de9737bebe").expect("valid thread id");
    let closed =
        ThreadId::from_string("019fcc09-cc5f-7af5-b0ec-88efa848cfcf").expect("valid thread id");
    navigation.upsert(
        current, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    navigation.upsert(
        open,
        Some("Robie".to_string()),
        Some("explorer".to_string()),
        /*is_closed*/ false,
    );
    navigation.upsert(
        closed,
        Some("Herschel".to_string()),
        Some("worker".to_string()),
        /*is_closed*/ true,
    );

    assert_eq!(
        agent_prompt_availability(&navigation, Some(current), Some(current), current),
        AgentPromptAvailability::Current("Main [default]".to_string())
    );
    assert_eq!(
        agent_prompt_availability(&navigation, Some(current), Some(current), open),
        AgentPromptAvailability::Available("Robie [explorer]".to_string())
    );
    assert_eq!(
        agent_prompt_availability(&navigation, Some(current), Some(current), closed),
        AgentPromptAvailability::Closed("Herschel [worker]".to_string())
    );
    assert_eq!(
        agent_prompt_availability(&navigation, Some(closed), Some(current), closed),
        AgentPromptAvailability::Closed("Herschel [worker]".to_string())
    );
    assert_eq!(
        agent_prompt_availability(
            &navigation,
            Some(current),
            Some(current),
            ThreadId::from_string("019fdd0a-dd6f-7b06-b1fd-9900b959d0d0").expect("valid thread id")
        ),
        AgentPromptAvailability::Unknown
    );
}

#[tokio::test]
async fn agent_prompt_uses_normal_composer_resource_resolution() {
    let mut app = super::super::test_support::make_test_app().await;
    let skill_path = PathBuf::from("/tmp/skills/review/SKILL.md").abs();
    app.chat_widget.set_skills(Some(vec![SkillMetadata {
        name: "review-skill".to_string(),
        description: "Review the implementation".to_string(),
        short_description: None,
        interface: None,
        dependencies: None,
        path: skill_path.clone(),
        scope: crate::test_support::skill_scope_repo(),
        enabled: true,
        plugin_id: None,
    }]));
    let inputs = app.chat_widget.user_inputs_from_message(&UserMessage {
        text: "Use $review-skill".to_string(),
        local_images: vec![LocalImageAttachment {
            placeholder: "[Image #1]".to_string(),
            path: PathBuf::from("/tmp/image.png"),
        }],
        remote_image_urls: vec!["data:image/png;base64,abc".to_string()],
        text_elements: vec![codex_protocol::user_input::TextElement::new(
            codex_protocol::user_input::ByteRange { start: 4, end: 17 },
            Some("$review-skill".to_string()),
        )],
        mention_bindings: Vec::new(),
    });

    assert_eq!(
        inputs,
        vec![
            UserInput::Image {
                url: "data:image/png;base64,abc".to_string(),
                detail: None,
            },
            UserInput::LocalImage {
                path: PathBuf::from("/tmp/image.png"),
                detail: None,
            },
            UserInput::Text {
                text: "Use $review-skill".to_string(),
                text_elements: vec![codex_app_server_protocol::TextElement::new(
                    codex_app_server_protocol::ByteRange { start: 4, end: 17 },
                    Some("$review-skill".to_string()),
                )],
            },
            UserInput::Skill {
                name: "review-skill".to_string(),
                path: skill_path.to_path_buf(),
            },
        ]
    );
}
