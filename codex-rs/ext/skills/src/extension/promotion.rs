//! Durable promoted inventories: rendering prepares state; publication acknowledges it.

use super::*;

pub(super) async fn contribute<C: Send + Sync + 'static>(
    extension: &SkillsExtension<C>,
    input: TurnInputContext<'_>,
    extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
    session_store: &ExtensionData,
    thread_store: &ExtensionData,
    turn_store: &ExtensionData,
) -> TurnInputContribution {
    let mut fragments = extension
        .contribute(
            input.clone(),
            extension_metrics.clone(),
            session_store,
            thread_store,
            turn_store,
        )
        .await;
    let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
        return TurnInputContribution::new(fragments);
    };

    let config = thread_state.config();
    let host_snapshot = turn_store.get::<HostSkillsSnapshot>();
    let mut catalog = extension
        .list_skills(
            SkillListQuery {
                turn_id: input.turn_id.clone(),
                executor_roots: Vec::new(),
                resolved_executor_roots: Vec::new(),
                host_snapshot,
                include_host_skills: true,
                include_bundled_skills: config.bundled_skills_enabled,
                include_orchestrator_skills: thread_state.orchestrator_skills_enabled(),
                mcp_resources: session_store
                    .get::<SkillsSessionState>()
                    .and_then(|state| state.mcp_resources.clone()),
                executor_capability_discovery: None,
            },
            &thread_state,
        )
        .await;
    // Keep durable promotion on the same provider precedence as turn-local injection.
    if let Some(executor_skills) = turn_store.get::<ExecutorSkillsStepState>() {
        catalog.extend(executor_skills.0.clone());
    }

    let mut selected_entries = collect_explicit_skill_mentions(&input.user_input, &catalog);
    let goal_mentions = thread_store
        .get::<ActiveGoalObjective>()
        .and_then(|active_goal| active_goal.snapshot())
        .map(|text| codex_protocol::user_input::UserInput::Text {
            text,
            text_elements: Vec::new(),
        })
        .into_iter()
        .collect::<Vec<_>>();
    for entry in collect_explicit_skill_mentions(&goal_mentions, &catalog) {
        if !selected_entries
            .iter()
            .any(|candidate| candidate.authority == entry.authority && candidate.id == entry.id)
        {
            selected_entries.push(entry);
        }
    }
    let promotion = prepare(&thread_state, &catalog, &selected_entries);
    if promotion.unresolved > 0 {
        extension.emit_unresolved_promotions_warning(
            thread_store.level_id(),
            Some(&input.turn_id),
            promotion.unresolved,
        );
    }
    if promotion.omitted > 0 {
        extension.emit_warning(
            thread_store.level_id(),
            Some(&input.turn_id),
            format!(
                "{} skill promotion(s) were omitted because the bounded promoted \
                 inventory is full.",
                promotion.omitted
            ),
        );
    }

    if !promotion.next.is_empty() && config.include_instructions {
        let mut promoted_catalog = catalog.clone();
        for entry in &mut promoted_catalog.entries {
            if promotion
                .entries
                .iter()
                .any(|promoted| promoted.authority == entry.authority && promoted.id == entry.id)
            {
                entry.prompt_visible = true;
            }
        }
        let include_usage = thread_store
            .get::<ModelInfo>()
            .is_some_and(|model_info| model_info.include_skills_usage_instructions);
        let rendered = render_catalog(
            extension_metrics.as_deref(),
            CatalogSurface::TurnInput,
            &promoted_catalog,
            include_usage,
            SkillCatalogRenderPolicy::ExtensionCompatible,
            skill_metadata_budget(
                thread_store
                    .get::<ModelInfo>()
                    .as_deref()
                    .and_then(ModelInfo::resolved_context_window),
                config.max_context_tokens,
            ),
        );
        let projected = rendered
            .included_identities
            .into_iter()
            .filter(|identity| promotion.next.contains(identity))
            .collect::<Vec<_>>();
        let replaces_baseline = fragments
            .iter()
            .any(|fragment| fragment.content_kind().0 == "skills.catalog");
        if let Some(fragment) = rendered.fragment
            && (promotion.changed
                || replaces_baseline
                || thread_state.promoted_projection_changed(&projected))
        {
            if let Some(message) = rendered.warning_message {
                extension.emit_warning(thread_store.level_id(), Some(&input.turn_id), message);
            }
            // One complete inventory supersedes the baseline from this contribution.
            // Omitted identities remain durable metadata, not acknowledged projection.
            fragments.retain(|fragment| fragment.content_kind().0 != "skills.catalog");
            fragments.push(Box::new(fragment.with_promoted(promotion.next.clone())));
            return TurnInputContribution::with_acknowledgement(fragments, move || {
                thread_state.acknowledge_promoted_skills(promotion.next, projected);
            });
        }
    }
    TurnInputContribution::new(fragments)
}

struct PromotionPlan {
    next: Vec<PromotedSkillIdentity>,
    entries: Vec<SkillCatalogEntry>,
    changed: bool,
    unresolved: usize,
    omitted: usize,
}

fn prepare(
    state: &SkillsThreadState,
    catalog: &SkillCatalog,
    selected_entries: &[SkillCatalogEntry],
) -> PromotionPlan {
    let promotable = selected_entries
        .iter()
        .filter(|entry| {
            !entry.prompt_visible
                && matches!(
                    entry.authority.kind,
                    SkillSourceKind::Host
                        | SkillSourceKind::Executor
                        | SkillSourceKind::Orchestrator
                )
        })
        .cloned()
        .collect::<Vec<_>>();
    let (next, changed, omitted) = state.promoted_with(&promotable);
    let (mut entries, unresolved) = state.resolve_promoted_skills(catalog);
    for entry in promotable {
        if !entries
            .iter()
            .any(|candidate| candidate.authority == entry.authority && candidate.id == entry.id)
        {
            entries.push(entry);
        }
    }
    entries.retain(|entry| next.iter().any(|identity| identity.matches_entry(entry)));
    PromotionPlan {
        next,
        entries,
        changed,
        unresolved,
        omitted,
    }
}
