use super::*;
use crate::context::ContextualUserFragment;
use crate::context::McpServerUseInstructions;
use codex_protocol::items::HookPromptFragment;
use codex_protocol::items::build_hook_prompt_message;
use codex_protocol::models::ResponseItem;

#[test]
fn detects_environment_context_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>".to_string(),
    }));
}

#[test]
fn detects_agents_instructions_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>"
            .to_string(),
    }));
}

#[test]
fn detects_subagent_notification_fragment_case_insensitively() {
    assert!(SubagentNotification::matches_text(
        "<SUBAGENT_NOTIFICATION>{}</subagent_notification>"
    ));
}

#[test]
fn ignores_regular_user_text() {
    assert!(!is_contextual_user_fragment(&ContentItem::InputText {
        text: "hello".to_string(),
    }));
}

#[test]
fn classifies_memory_excluded_fragments() {
    let cases = [
        (
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
            true,
        ),
        (
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            false,
        ),
        (
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
            false,
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(
            is_memory_excluded_contextual_user_fragment(&ContentItem::InputText {
                text: text.to_string(),
            }),
            expected,
            "{text}",
        );
    }
}

#[test]
fn detects_hook_prompt_fragment_and_roundtrips_escaping() {
    let message = build_hook_prompt_message(&[HookPromptFragment::from_single_hook(
        r#"Retry with "waves" & <tides>"#,
        "hook-run-1",
    )])
    .expect("hook prompt message");

    let ResponseItem::Message { content, .. } = message else {
        panic!("expected hook prompt response item");
    };

    let [content_item] = content.as_slice() else {
        panic!("expected a single content item");
    };

    assert!(is_contextual_user_fragment(content_item));

    let ContentItem::InputText { text } = content_item else {
        panic!("expected input text content item");
    };
    let parsed = parse_visible_hook_prompt_message(/*id*/ None, content.as_slice())
        .expect("visible hook prompt");
    assert_eq!(
        parsed.fragments,
        vec![HookPromptFragment {
            text: r#"Retry with "waves" & <tides>"#.to_string(),
            hook_run_id: "hook-run-1".to_string(),
        }],
    );
    assert!(!text.contains("&quot;waves&quot; & <tides>"));
}

#[test]
fn mcp_use_fragment_hard_product_requirement_preserves_full_verbatim_inventory_without_any_truncation()
 {
    let tool_names = (0..100)
        .map(|index| format!("tool_{index}_{}", "x".repeat(400)))
        .collect::<Vec<_>>();
    let fragment = McpServerUseInstructions::new("acme docs".to_string(), tool_names.clone());
    let rendered = fragment.render();

    assert!(McpServerUseInstructions::matches_text(&rendered));
    assert_eq!(
        McpServerUseInstructions::parse_server_name(&rendered),
        Some("acme docs".to_string())
    );
    for tool_name in tool_names {
        assert!(
            rendered.contains(tool_name.as_str()),
            "HARD PRODUCT REQUIREMENT: /mcp use is explicit user-directed context expansion and must preserve the full verbatim MCP tool inventory; adding token caps, byte caps, tool-count caps, truncation, summarization, abbreviation, hashing, pagination, or omission here is a product-contract violation"
        );
    }
    assert!(
        rendered.len() > 40 * 1024,
        "this test intentionally uses a large fragment so future reviewers can see that no-cap behavior is deliberate product policy"
    );
    assert!(
        is_contextual_user_fragment(&ContentItem::InputText { text: rendered }),
        "expected /mcp use scaffolding to remain recognized contextual prompt content"
    );
}

#[test]
fn mcp_use_fragment_roundtrips_server_names_with_backticks() {
    let fragment = McpServerUseInstructions::new("foo`bar".to_string(), vec!["tool".to_string()]);
    let rendered = fragment.render();
    assert_eq!(
        McpServerUseInstructions::parse_server_name(&rendered),
        Some("foo`bar".to_string())
    );
}
