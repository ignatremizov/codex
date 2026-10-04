use super::*;
use codex_history::CodexHarnessMetadata;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ReasoningEffortPreset;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn handoff_decode_requires_remote_compaction_feature_and_internal_flag() {
    let mut config = crate::config::test_config().await;
    config.remote_compaction_handoff_enabled = true;
    config
        .features
        .disable(Feature::RemoteCompaction)
        .expect("disable remote compaction");
    assert!(!should_decode_remote_compaction_handoff(&config));

    config
        .features
        .enable(Feature::RemoteCompaction)
        .expect("enable remote compaction");
    assert!(should_decode_remote_compaction_handoff(&config));

    config.remote_compaction_handoff_enabled = false;
    assert!(!should_decode_remote_compaction_handoff(&config));
}

#[tokio::test]
async fn cancelled_handoff_decode_wins_over_invalid_helper_config() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let mut config = turn_context.config.as_ref().clone();
    config.remote_compaction_handoff_enabled = true;
    config
        .features
        .enable(Feature::RemoteCompaction)
        .expect("enable remote compaction");
    config.web_search_mode = Constrained::allow_only(WebSearchMode::Live);
    let selection = HandoffModelSelection {
        model: "helper-model".to_string(),
        reasoning_effort: Some(ReasoningEffort::Low),
    };
    assert!(
        build_remote_compaction_handoff_config(&config, &selection).is_err(),
        "test setup must make helper configuration invalid"
    );
    turn_context.config = Arc::new(config);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert_eq!(
        summarize_remote_compaction_handoff(
            &Arc::new(session),
            &Arc::new(turn_context),
            &[],
            &cancellation,
        )
        .await,
        RemoteCompactionHandoff::Skipped,
    );
}

fn preset(model: &str, default: ReasoningEffort, supported: &[ReasoningEffort]) -> ModelPreset {
    ModelPreset {
        id: model.to_string(),
        model: model.to_string(),
        display_name: model.to_string(),
        description: String::new(),
        model_specialty: None,
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
        available_access_programs: None,
        is_default: false,
        upgrade: None,
        show_in_picker: true,
        multi_agent_version: None,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: vec![InputModality::Text],
    }
}

#[test]
fn selection_prefers_config_then_catalog_then_current_with_supported_reasoning() {
    let catalog = vec![
        preset(
            "configured",
            ReasoningEffort::High,
            &[ReasoningEffort::Low, ReasoningEffort::High],
        ),
        preset(
            DEFAULT_HANDOFF_MODEL,
            ReasoningEffort::Medium,
            &[ReasoningEffort::Medium],
        ),
    ];
    for (configured, expected) in [
        (
            Some(" configured "),
            HandoffModelSelection {
                model: "configured".to_string(),
                reasoning_effort: Some(ReasoningEffort::Low),
            },
        ),
        (
            Some("  "),
            HandoffModelSelection {
                model: DEFAULT_HANDOFF_MODEL.to_string(),
                reasoning_effort: Some(ReasoningEffort::Medium),
            },
        ),
    ] {
        assert_eq!(
            select_handoff_model(
                configured,
                &catalog,
                "current",
                Some(ReasoningEffort::High),
                /*current_supports_low*/ true
            ),
            expected
        );
    }
    assert_eq!(
        select_handoff_model(
            /*configured*/ None,
            &[],
            "current",
            Some(ReasoningEffort::High),
            /*current_supports_low*/ false
        ),
        HandoffModelSelection {
            model: "current".to_string(),
            reasoning_effort: Some(ReasoningEffort::High)
        },
    );
    assert_eq!(
        select_handoff_model(
            /*configured*/ None,
            &[],
            "current",
            Some(ReasoningEffort::High),
            /*current_supports_low*/ true
        ),
        HandoffModelSelection {
            model: "current".to_string(),
            reasoning_effort: Some(ReasoningEffort::Low)
        },
    );
}

#[test]
fn fallback_prefers_config_and_skips_unavailable_or_duplicate_models() {
    let primary = HandoffModelSelection {
        model: "primary".to_string(),
        reasoning_effort: None,
    };
    let catalog = vec![preset(
        DEFAULT_HANDOFF_FALLBACK_MODEL,
        ReasoningEffort::High,
        &[ReasoningEffort::Low],
    )];
    assert_eq!(
        select_handoff_fallback_model(Some(" custom "), &catalog, &primary),
        Some(HandoffModelSelection {
            model: "custom".to_string(),
            reasoning_effort: None
        }),
    );
    assert_eq!(
        select_handoff_fallback_model(Some(" "), &catalog, &primary),
        Some(HandoffModelSelection {
            model: DEFAULT_HANDOFF_FALLBACK_MODEL.to_string(),
            reasoning_effort: Some(ReasoningEffort::Low)
        }),
    );
    assert_eq!(
        select_handoff_fallback_model(/*configured*/ None, &[], &primary),
        None
    );
    assert_eq!(
        select_handoff_fallback_model(Some(" primary "), &catalog, &primary),
        None
    );
}

#[test]
fn decoder_seed_preserves_every_envelope_and_appends_only_the_developer_instruction() {
    let mut checkpoint = ResponseItemEnvelope::new(ResponseItem::Compaction {
        id: Some(codex_protocol::ResponseItemId::with_suffix(
            "cmp",
            "installed-checkpoint",
        )),
        encrypted_content: "opaque".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    checkpoint.metadata = Some(CodexHarnessMetadata {
        compaction_model_hash: Some("installed-hash".to_string()),
        ..Default::default()
    });
    let InitialHistory::Forked(items) =
        build_handoff_initial_history(std::slice::from_ref(&checkpoint))
    else {
        panic!("expected forked seed");
    };
    let items = items
        .into_iter()
        .map(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope,
            _ => panic!("unexpected decoder seed record"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        items,
        vec![
            checkpoint,
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: HANDOFF_PROMPT.to_string()
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ]
    );
}

#[test]
fn unusable_decoder_text_is_rejected_without_trimming_valid_output() {
    for value in ["", " \n ", HANDOFF_PROMPT] {
        assert_eq!(usable_handoff_message(value.to_string()), None);
    }
    assert_eq!(
        usable_handoff_message("  decoded\n".to_string()),
        Some("  decoded\n".to_string())
    );
}

#[tokio::test]
async fn decoder_overrides_parent_instruction_provenance_and_side_effects() {
    let mut parent = crate::config::test_config().await;
    parent.developer_instructions = Some("parent private instructions".to_string());
    parent
        .features
        .enable(Feature::CodexHooks)
        .expect("enable parent hooks");
    parent
        .features
        .enable(Feature::Plugins)
        .expect("enable parent plugins");
    parent
        .features
        .enable(Feature::ContextManagement)
        .expect("enable parent context management");
    parent
        .features
        .enable(Feature::CodeModePrewarm)
        .expect("enable parent Code Mode prewarming");
    let config = build_remote_compaction_handoff_config(
        &parent,
        &HandoffModelSelection {
            model: "decoder".to_string(),
            reasoning_effort: Some(ReasoningEffort::Low),
        },
    )
    .expect("decoder config");
    assert_eq!(
        (
            config.base_instructions,
            config.base_instructions_provenance,
            config.developer_instructions
        ),
        (
            Some(HANDOFF_PROMPT.to_string()),
            Some(BaseInstructionsProvenance::Custom),
            None
        ),
    );
    assert_eq!(
        (
            config.ephemeral,
            config.remote_compaction_handoff_enabled,
            config.include_skill_instructions,
            config.include_environment_context,
            config.model_post_turn_compact_threshold_percent
        ),
        (true, false, false, false, 0),
    );
    assert!(!config.features.enabled(Feature::CodexHooks));
    assert!(!config.features.enabled(Feature::Plugins));
    assert!(!config.features.enabled(Feature::ContextManagement));
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert!(!config.features.enabled(Feature::CodeModePrewarm));
    assert!(parent.features.enabled(Feature::ContextManagement));
    assert!(parent.features.enabled(Feature::CodeModePrewarm));
    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::Never
    );
    assert_eq!(
        config.permissions.effective_permission_profile(),
        PermissionProfile::read_only()
    );
}
