//! Durable promoted inventories: rendering prepares state; publication acknowledges it.

use super::*;
use codex_protocol::models::ContentItem;
use std::sync::Mutex;

#[derive(Default)]
struct PublishedTurnInventory(Mutex<Option<ContentItem>>);

struct PromotionAcknowledgement {
    state: Arc<SkillsThreadState>,
    identities: Vec<PromotedSkillIdentity>,
    projected: Vec<PromotedSkillIdentity>,
}

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
    let Some(step_state) = input.step_store.get::<SkillsStepState>() else {
        return TurnInputContribution::new(fragments);
    };
    let mut catalog = SkillCatalog::default();
    if let Some(host_catalog) = &step_state.host_catalog {
        catalog.extend(host_catalog.clone());
    } else {
        catalog.extend(
            extension
                .providers
                .list_for_turn(SkillListQuery {
                    turn_id: input.turn_id.clone(),
                    executor_roots: Vec::new(),
                    resolved_executor_roots: Vec::new(),
                    host_snapshot: turn_store.get::<HostSkillsSnapshot>(),
                    include_host_skills: true,
                    include_bundled_skills: config.bundled_skills_enabled,
                    include_cloud_skills: false,
                    mcp_resources: session_store
                        .get::<SkillsSessionState>()
                        .and_then(|state| state.mcp_resources.clone()),
                    executor_capability_discovery: None,
                    executor_capability_discovery_v2: None,
                })
                .await,
        );
    }
    catalog.extend(step_state.catalog.merged.clone());

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

    if config.include_instructions && !promotion.next.is_empty() {
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
            return durable_inventory_contribution(
                fragments,
                turn_store,
                Some(PromotionAcknowledgement {
                    state: thread_state,
                    identities: promotion.next,
                    projected,
                }),
            );
        }
    }
    durable_inventory_contribution(fragments, turn_store, /*promotion*/ None)
}

// A retry may prepare the same inventory again, but only durable publication suppresses it.
// Keep this receipt turn-scoped so a fresh turn can establish its own initial inventory.
fn durable_inventory_contribution(
    mut fragments: Vec<Box<dyn ContextualUserFragment + Send>>,
    turn_store: &ExtensionData,
    promotion: Option<PromotionAcknowledgement>,
) -> TurnInputContribution {
    let Some(inventory) = fragments
        .iter()
        .find(|fragment| fragment.content_kind().0 == "skills.catalog")
        .map(|fragment| fragment.render())
    else {
        return TurnInputContribution::new(fragments);
    };
    let published = turn_store.get_or_init(PublishedTurnInventory::default);
    let unchanged = published
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        == Some(&inventory);
    if unchanged {
        fragments.retain(|fragment| fragment.content_kind().0 != "skills.catalog");
        return TurnInputContribution::new(fragments);
    }
    TurnInputContribution::with_acknowledgement(fragments, move || {
        *published
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(inventory);
        if let Some(promotion) = promotion {
            promotion
                .state
                .acknowledge_promoted_skills(promotion.identities, promotion.projected);
        }
    })
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
            matches!(
                entry.authority.kind,
                SkillSourceKind::Host | SkillSourceKind::Executor | SkillSourceKind::Cloud
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
