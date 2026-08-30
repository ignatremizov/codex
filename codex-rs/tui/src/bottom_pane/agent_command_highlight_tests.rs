use super::*;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
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

fn models() -> Vec<ModelPreset> {
    vec![ModelPreset {
        id: "gpt-5.6-sol".to_string(),
        model: "gpt-5.6-sol".to_string(),
        display_name: "GPT-5.6 Sol".to_string(),
        description: "Frontier coding model".to_string(),
        model_specialty: None,
        default_reasoning_effort: ReasoningEffort::Medium,
        supported_reasoning_efforts: vec![
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffort::Medium,
                description: "Balanced reasoning".to_string(),
            },
            codex_protocol::openai_models::ReasoningEffortPreset {
                effort: ReasoningEffort::High,
                description: "Deeper reasoning".to_string(),
            },
        ],
        supports_personality: false,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        is_default: true,
        upgrade: None,
        show_in_picker: true,
        multi_agent_version: None,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: codex_protocol::openai_models::default_input_modalities(),
    }]
}

fn highlighted_tokens(input: &str) -> Vec<(&str, AgentCommandHighlightKind)> {
    agent_command_highlights(input, &targets(), &models())
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
        highlighted_tokens(
            "/agent reviewer model:gpt-5.6-sol effort:high fork:none w:cf review close carefully"
        ),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("reviewer", AgentCommandHighlightKind::KnownTarget),
            ("model:gpt-5.6-sol", AgentCommandHighlightKind::Option),
            ("effort:high", AgentCommandHighlightKind::Option),
            ("fork:none", AgentCommandHighlightKind::Option),
            ("w:cf", AgentCommandHighlightKind::Option),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent reviewer model:\"gpt-5.6-sol\" effort:high prompt"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("reviewer", AgentCommandHighlightKind::KnownTarget),
            ("model:\"gpt-5.6-sol\"", AgentCommandHighlightKind::Option),
            ("effort:high", AgentCommandHighlightKind::Option),
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
    assert_eq!(
        highlighted_tokens("/agent reviewer effort:banana prompt"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("reviewer", AgentCommandHighlightKind::KnownTarget),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent reviewer model:missing effort:high prompt"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("reviewer", AgentCommandHighlightKind::KnownTarget),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent queue 2 effort:high prompt"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("queue", AgentCommandHighlightKind::Action),
            ("2", AgentCommandHighlightKind::KnownTarget),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent 2 model:gpt-5.6-sol prompt"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("2", AgentCommandHighlightKind::KnownTarget),
        ]
    );
    assert_eq!(
        highlighted_tokens("/agent observe 2 w:f"),
        vec![
            ("/agent", AgentCommandHighlightKind::Command),
            ("observe", AgentCommandHighlightKind::Action),
            ("2", AgentCommandHighlightKind::KnownTarget),
        ]
    );
}
