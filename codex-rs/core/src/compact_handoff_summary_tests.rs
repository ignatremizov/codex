use super::*;
use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ReasoningEffortPreset;
use pretty_assertions::assert_eq;

fn preset(model: &str, default: ReasoningEffort, supported: &[ReasoningEffort]) -> ModelPreset {
    ModelPreset {
        id: model.to_string(),
        model: model.to_string(),
        display_name: model.to_string(),
        description: String::new(),
        default_reasoning_effort: default,
        supported_reasoning_efforts: supported
            .iter()
            .cloned()
            .map(|effort| ReasoningEffortPreset {
                effort,
                description: String::new(),
            })
            .collect(),
        supports_personality: false,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        is_default: false,
        upgrade: None,
        show_in_picker: true,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: vec![InputModality::Text],
    }
}

#[test]
fn select_handoff_model_uses_configured_model_first() {
    let selection = select_handoff_model(
        Some("configured-model"),
        &[preset(
            "configured-model",
            ReasoningEffort::High,
            &[ReasoningEffort::High, ReasoningEffort::Low],
        )],
        "current-model",
        Some(ReasoningEffort::Medium),
        Some(ReasoningEffort::Medium),
        /*current_supports_low_reasoning*/ true,
    );

    assert_eq!(
        selection,
        HandoffModelSelection {
            model: "configured-model".to_string(),
            reasoning_effort: Some(ReasoningEffort::Low),
        }
    );
}

#[test]
fn select_handoff_model_uses_spark_when_available() {
    let selection = select_handoff_model(
        /*configured_model*/ None,
        &[preset(
            DEFAULT_HANDOFF_MODEL,
            ReasoningEffort::Medium,
            &[ReasoningEffort::Medium],
        )],
        "current-model",
        Some(ReasoningEffort::High),
        Some(ReasoningEffort::High),
        /*current_supports_low_reasoning*/ true,
    );

    assert_eq!(
        selection,
        HandoffModelSelection {
            model: DEFAULT_HANDOFF_MODEL.to_string(),
            reasoning_effort: Some(ReasoningEffort::Medium),
        }
    );
}

#[test]
fn select_handoff_model_falls_back_to_current_model() {
    let selection = select_handoff_model(
        /*configured_model*/ None,
        &[],
        "current-model",
        Some(ReasoningEffort::High),
        Some(ReasoningEffort::Medium),
        /*current_supports_low_reasoning*/ false,
    );

    assert_eq!(
        selection,
        HandoffModelSelection {
            model: "current-model".to_string(),
            reasoning_effort: Some(ReasoningEffort::High),
        }
    );
}

#[test]
fn select_handoff_model_uses_low_for_current_model_when_supported() {
    let selection = select_handoff_model(
        /*configured_model*/ None,
        &[],
        "current-model",
        Some(ReasoningEffort::High),
        Some(ReasoningEffort::Medium),
        /*current_supports_low_reasoning*/ true,
    );

    assert_eq!(selection.reasoning_effort, Some(ReasoningEffort::Low));
}

#[test]
fn usable_handoff_message_rejects_prompt_echo() {
    assert_eq!(usable_handoff_message(HANDOFF_PROMPT.to_string()), None);
    assert_eq!(
        usable_handoff_message("decoded handoff".to_string()),
        Some("decoded handoff".to_string())
    );
}

#[test]
fn handoff_initial_history_appends_instruction_after_new_history() {
    let summary = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "compacted handoff".to_string(),
        }],
        phase: None,
    };

    let items = build_handoff_initial_history(std::slice::from_ref(&summary));

    let Some(RolloutItem::ResponseItem(first_item)) = items.first() else {
        panic!("expected compacted history first");
    };
    assert_eq!(first_item, &summary);
    let Some(RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })) = items.last()
    else {
        panic!("expected trailing handoff instruction");
    };
    assert_eq!(role, "developer");
    assert_eq!(
        content.as_slice(),
        &[ContentItem::InputText {
            text: HANDOFF_PROMPT.to_string(),
        }]
    );
}

#[tokio::test]
async fn handoff_config_is_locked_down_and_ephemeral() {
    let mut parent_config = crate::config::test_config().await;
    parent_config.include_apps_instructions = true;
    parent_config.include_skill_instructions = true;
    parent_config.include_environment_context = true;
    parent_config.developer_instructions = Some("parent dev".to_string());
    parent_config.personality = Some(Personality::Friendly);
    parent_config
        .features
        .enable(Feature::CodexHooks)
        .expect("enable hooks on parent config");
    parent_config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable multi-agent on parent config");

    let helper_config = build_remote_compaction_handoff_config(
        &parent_config,
        &HandoffModelSelection {
            model: "helper-model".to_string(),
            reasoning_effort: Some(ReasoningEffort::Low),
        },
    )
    .expect("handoff helper config");

    assert!(helper_config.ephemeral);
    assert_eq!(helper_config.model.as_deref(), Some("helper-model"));
    assert_eq!(
        helper_config.model_reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    assert_eq!(
        helper_config.base_instructions.as_deref(),
        Some(HANDOFF_PROMPT)
    );
    assert!(!helper_config.remote_compaction_handoff_enabled);
    assert_eq!(helper_config.developer_instructions, None);
    assert_eq!(helper_config.personality, None);
    assert_eq!(helper_config.project_doc_max_bytes, 0);
    assert!(helper_config.project_doc_fallback_filenames.is_empty());
    assert!(!helper_config.include_apps_instructions);
    assert!(!helper_config.include_skill_instructions);
    assert!(!helper_config.include_environment_context);
    assert_eq!(
        helper_config.permissions.approval_policy.value(),
        AskForApproval::Never
    );
    assert_eq!(
        helper_config.permissions.effective_permission_profile(),
        PermissionProfile::read_only()
    );
    assert!(helper_config.mcp_servers.get().is_empty());
    assert!(!helper_config.features.enabled(Feature::Personality));
    assert!(!helper_config.features.enabled(Feature::CodexHooks));
    assert!(!helper_config.features.enabled(Feature::MultiAgentV2));
    assert!(!helper_config.features.enabled(Feature::Plugins));
    assert!(!helper_config.features.enabled(Feature::RemoteCompaction));
    assert!(!helper_config.features.enabled(Feature::RemoteCompactionV2));
    assert!(!helper_config.features.enabled(Feature::RemotePlugin));
    assert!(!helper_config.features.enabled(Feature::ShellTool));
}
