use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn steered_promotion_is_acknowledged_only_after_recording_and_does_not_reset_inventory()
-> TestResult {
    let mut builder = ExtensionRegistryBuilder::new();
    let read_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = |entries| {
        Arc::new(StaticSkillProvider {
            catalog: SkillCatalog {
                entries,
                warnings: Vec::new(),
            },
            read_requests: Arc::clone(&read_requests),
            list_calls: None,
            fail_first_list: false,
        })
    };
    install_with_providers(
        &mut builder,
        SkillProviders::new()
            .with_host_provider(provider(vec![SkillCatalogEntry::new(
                SkillPackageId("host-visible".to_string()),
                SkillAuthority::new(SkillSourceKind::Host, "host"),
                "visible",
                "Visible host skill.",
                SkillResourceId::new("visible/SKILL.md"),
            )]))
            .with_orchestrator_provider(provider(vec![
                SkillCatalogEntry::new(
                    SkillPackageId("skill://demo/unwrap".to_string()),
                    SkillAuthority::new(SkillSourceKind::Orchestrator, "codex_apps"),
                    "unwrap",
                    "Explicit-only orchestrator skill.",
                    SkillResourceId::new("skill://demo/unwrap"),
                )
                .hidden_from_prompt(),
            ])),
        skills_extension_config,
    );
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let turn_store = ExtensionData::new("turn");
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &default_config(),
            session_source: &SessionSource::Cli,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    let contributor = &registry.turn_input_contributors()[0];
    let initial = contributor
        .contribute_durable(
            TurnInputContext {
                turn_id: "turn".to_string(),
                user_input: Vec::new(),
                environments: Vec::new(),
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &turn_store,
        )
        .await;
    let (fragments, acknowledgement) = initial.into_parts();
    assert!(acknowledgement.is_none());
    assert_eq!(fragments.len(), 1);
    assert!(fragments[0].render().contains("- visible:"));
    assert!(!fragments[0].render().contains("- unwrap:"));

    let input = TurnInputContext {
        turn_id: "turn".to_string(),
        user_input: vec![UserInput::Text {
            text: "Use $unwrap.".to_string(),
            text_elements: Vec::new(),
        }],
        environments: Vec::new(),
    };
    let mut recorded = Vec::new();
    for attempt in 0..3 {
        let contribution = contributor
            .contribute_durable(
                input.clone(),
                /*extension_metrics*/ None,
                &session_store,
                &thread_store,
                &turn_store,
            )
            .await;
        let (fragments, acknowledgement) = contribution.into_parts();
        let rendered = fragments
            .iter()
            .map(|fragment| (fragment.role(), fragment.render()))
            .collect::<Vec<_>>();
        match attempt {
            0 => {
                assert_eq!(rendered.len(), 1);
                assert_eq!(rendered[0].0, "developer");
                assert!(rendered[0].1.contains("- unwrap:"));
                assert!(rendered[0].1.contains("- visible:"));
                assert!(acknowledgement.is_some());
                recorded = rendered;
                // Cancellation before durable recording must leave promotion retryable.
                drop(acknowledgement);
            }
            1 => {
                assert_eq!(rendered, recorded);
                acknowledgement
                    .ok_or("promotion must remain pending")?
                    .acknowledge();
            }
            2 => {
                assert_eq!(rendered, Vec::<(&str, String)>::new());
                assert!(acknowledgement.is_none());
            }
            _ => unreachable!("three contribution attempts"),
        }
    }
    let next_turn = ExtensionData::new("next-turn");
    let contribution = contributor
        .contribute_durable(
            TurnInputContext {
                turn_id: "next-turn".to_string(),
                user_input: Vec::new(),
                environments: Vec::new(),
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &next_turn,
        )
        .await;
    let (fragments, acknowledgement) = contribution.into_parts();
    let inventory = fragments
        .iter()
        .map(|fragment| fragment.render())
        .collect::<Vec<_>>();
    assert_eq!(inventory.len(), 1);
    assert!(inventory[0].contains("- visible:"));
    assert!(inventory[0].contains("- unwrap:"));
    acknowledgement
        .ok_or("replacement inventory must be acknowledged")?
        .acknowledge();
    // Orchestrator promotion advertises the resource route without loading its body.
    assert!(read_requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn bounded_inventory_keeps_visible_entries_and_reprojects_omitted_promotions() -> TestResult {
    let mut builder = ExtensionRegistryBuilder::new();
    let read_requests = Arc::new(Mutex::new(Vec::new()));
    let hidden_package = format!("skill://demo/{}", "x".repeat(220));
    install_with_providers(
        &mut builder,
        SkillProviders::new().with_orchestrator_provider(Arc::new(StaticSkillProvider {
            catalog: SkillCatalog {
                entries: vec![
                    SkillCatalogEntry::new(
                        SkillPackageId("short".to_string()),
                        SkillAuthority::new(SkillSourceKind::Orchestrator, "codex_apps"),
                        "visible",
                        "Visible inventory entry.",
                        SkillResourceId::new("short"),
                    ),
                    SkillCatalogEntry::new(
                        SkillPackageId(hidden_package.clone()),
                        SkillAuthority::new(SkillSourceKind::Orchestrator, "codex_apps"),
                        "later",
                        "Explicitly selected but initially omitted by the catalog budget.",
                        SkillResourceId::new(hidden_package),
                    )
                    .hidden_from_prompt(),
                ],
                warnings: Vec::new(),
            },
            read_requests: Arc::clone(&read_requests),
            list_calls: None,
            fail_first_list: false,
        })),
        |tokens: &usize| SkillsExtensionConfig {
            include_instructions: true,
            max_context_tokens: std::num::NonZeroUsize::new(*tokens),
            bundled_skills_enabled: true,
            orchestrator_skills_enabled: true,
            shadow_selection_enabled: false,
        },
    );
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let turn_store = ExtensionData::new("turn");
    let small_budget = 60;
    let large_budget = 256;
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &small_budget,
            session_source: &SessionSource::Cli,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    let contributor = &registry.turn_input_contributors()[0];
    let input = TurnInputContext {
        turn_id: "turn".to_string(),
        user_input: vec![UserInput::Text {
            text: "Use $later.".to_string(),
            text_elements: Vec::new(),
        }],
        environments: Vec::new(),
    };
    for (attempt, includes_later) in [(0, false), (1, true)] {
        let contribution = contributor
            .contribute_durable(
                input.clone(),
                /*extension_metrics*/ None,
                &session_store,
                &thread_store,
                &turn_store,
            )
            .await;
        let (fragments, acknowledgement) = contribution.into_parts();
        let inventory = fragments
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>();
        assert_eq!(inventory.len(), 1);
        assert!(inventory[0].contains("- visible:"));
        assert_eq!(inventory[0].contains("- later:"), includes_later);
        assert_eq!(
            inventory[0].contains("additional skill omitted"),
            !includes_later
        );
        acknowledgement
            .ok_or("published inventory must acknowledge its actual projection")?
            .acknowledge();

        let repeated = contributor
            .contribute_durable(
                input.clone(),
                /*extension_metrics*/ None,
                &session_store,
                &thread_store,
                &turn_store,
            )
            .await;
        let (fragments, acknowledgement) = repeated.into_parts();
        assert!(
            fragments.is_empty(),
            "unchanged projection should not repeat on attempt {attempt}"
        );
        assert!(acknowledgement.is_none());
        registry.config_contributors()[0].on_config_changed(
            &session_store,
            &thread_store,
            &small_budget,
            &large_budget,
        );
    }
    assert!(read_requests.lock().unwrap().is_empty());
    Ok(())
}
