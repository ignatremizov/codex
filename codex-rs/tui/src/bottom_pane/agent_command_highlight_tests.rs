use super::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

fn targets() -> Vec<AgentPromptTarget> {
    vec![
        AgentPromptTarget {
            thread_id: Some(
                ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad")
                    .expect("valid thread id"),
            ),
            selector: "2".to_string(),
            label: "Sagan [default]".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "reviewer".to_string(),
            label: "New reviewer agent".to_string(),
        },
    ]
}

fn highlighted_tokens(input: &str) -> Vec<(&str, AgentCommandHighlightKind)> {
    agent_command_highlights(input, &targets())
        .into_iter()
        .map(|highlight| (&input[highlight.range], highlight.kind))
        .collect()
}

#[test]
fn highlights_action_first_and_target_first_agent_commands() {
    assert_eq!(
        highlighted_tokens("/agent close 2 w:x"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("close", AgentCommandHighlightKind::Action),
            ("2", AgentCommandHighlightKind::KnownTarget),
            ("w:x", AgentCommandHighlightKind::Option),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent Sagan close w:x"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("Sagan", AgentCommandHighlightKind::KnownTarget),
            ("close", AgentCommandHighlightKind::Action),
            ("w:x", AgentCommandHighlightKind::Option),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent role:\"reviewer\" fork:none"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("role:\"reviewer\"", AgentCommandHighlightKind::KnownTarget,),
            ("fork:none", AgentCommandHighlightKind::Option),
        ]
    );
}

#[test]
fn highlights_spawn_options_but_leaves_prompt_text_plain() {
    assert_eq!(
        highlighted_tokens("/agent reviewer fork:none w:cf review close carefully"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("reviewer", AgentCommandHighlightKind::KnownTarget),
            ("fork:none", AgentCommandHighlightKind::Option),
            ("w:cf", AgentCommandHighlightKind::Option),
        ]
    );
}

#[test]
fn distinguishes_unresolved_targets_and_rejects_invalid_options() {
    assert_eq!(
        highlighted_tokens("/agent missing w:fc prompt"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("missing", AgentCommandHighlightKind::UnknownTarget),
        ]
    );
}
