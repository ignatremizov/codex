use super::*;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::is_model_generated_item;
use crate::context_manager::is_user_turn_boundary;
use codex_protocol::protocol::SessionContextWindow;
use uuid::Uuid;

// Return value of `Session::reconstruct_history_from_rollout`, bundling the rebuilt history with
// the resume/fork hydration metadata derived from the same replay.
#[derive(Debug)]
pub(super) struct RolloutReconstruction {
    pub(super) history: Vec<ResponseItem>,
    pub(super) compacted_prefix_len: Option<usize>,
    pub(super) repair: Option<RolloutReconstructionRepair>,
    pub(super) should_recompute_token_usage: bool,
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Option<Uuid>,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Option<Uuid>,
}

#[derive(Debug)]
pub(super) struct RolloutReconstructionRepair {
    pub(super) checkpoint: CompactedItem,
    pub(super) sanitization: crate::context::CompactedMediaSanitization,
    pub(super) persistence: RolloutReconstructionRepairPersistence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RolloutReconstructionRepairPersistence {
    Required,
    BestEffort,
}

#[derive(Debug)]
pub(super) struct AppliedRolloutReconstructionRepair {
    pub(super) items: Vec<RolloutItem>,
    pub(super) sanitization: crate::context::CompactedMediaSanitization,
    pub(super) persistence: RolloutReconstructionRepairPersistence,
}

#[derive(Debug)]
pub(super) struct AppliedRolloutReconstruction {
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) repair: Option<AppliedRolloutReconstructionRepair>,
    pub(super) should_recompute_token_usage: bool,
}

#[derive(Debug)]
pub(super) struct PreparedRolloutReconstruction {
    pub(super) history: Vec<ResponseItem>,
    pub(super) compacted_prefix_len: Option<usize>,
    pub(super) repair: Option<AppliedRolloutReconstructionRepair>,
    pub(super) should_recompute_token_usage: bool,
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Uuid,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Uuid,
}

#[derive(Debug, Clone, Copy)]
struct ReconstructedWindow {
    number: u64,
    first_id: Option<Uuid>,
    previous_id: Option<Uuid>,
    id: Option<Uuid>,
}

#[derive(Debug, Default)]
enum TurnReferenceContextItem {
    /// No `TurnContextItem` has been seen for this replay span yet.
    ///
    /// This differs from `Cleared`: `NeverSet` means there is no evidence this turn ever
    /// established a baseline, while `Cleared` means a baseline existed and a later compaction
    /// invalidated it. Only the latter must emit an explicit clearing segment for resume/fork
    /// hydration.
    #[default]
    NeverSet,
    /// A previously established baseline was invalidated by later compaction.
    Cleared,
    /// The latest baseline established by this replay span.
    Latest(Box<TurnContextItem>),
}

#[derive(Debug, Default)]
struct ActiveReplaySegment<'a> {
    turn_id: Option<String>,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    compacted_items: Vec<&'a CompactedItem>,
    base_compacted_item: Option<&'a CompactedItem>,
    rollout_suffix: Option<&'a [RolloutItem]>,
    window: Option<ReconstructedWindow>,
}

#[derive(Debug, Default)]
struct ReverseReplayState<'a> {
    base_compacted_item: Option<&'a CompactedItem>,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    world_state_boundary_known: bool,
    window: Option<ReconstructedWindow>,
    pending_rollback_turns: usize,
    rollout_suffix: Option<&'a [RolloutItem]>,
    skipped_compacted_items: Vec<&'a CompactedItem>,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn finalize_active_segment<'a>(
    active_segment: ActiveReplaySegment<'a>,
    replay_state: &mut ReverseReplayState<'a>,
) {
    // Thread rollback drops the newest surviving real user-message boundaries. In replay, that
    // means skipping the next finalized segments that contain a non-contextual
    // `EventMsg::UserMessage`.
    if replay_state.pending_rollback_turns > 0 {
        replay_state
            .skipped_compacted_items
            .extend(active_segment.compacted_items);
        if active_segment.counts_as_user_turn {
            replay_state.pending_rollback_turns -= 1;
        }
        return;
    }

    replay_state.world_state_boundary_known |= active_segment
        .world_state_replay
        .iter()
        .any(|item| establishes_world_state_boundary(item));
    replay_state
        .world_state_replay
        .extend(active_segment.world_state_replay);

    // A surviving replacement-history checkpoint is a complete history base. Once we
    // know the newest surviving one, older rollout items do not affect rebuilt history.
    if replay_state.base_compacted_item.is_none()
        && let Some(segment_base_compacted_item) = active_segment.base_compacted_item
    {
        replay_state.base_compacted_item = Some(segment_base_compacted_item);
        replay_state.rollout_suffix = active_segment.rollout_suffix;
    }

    if replay_state.window.is_none() {
        replay_state.window = active_segment.window;
    }

    // `previous_turn_settings` come from the newest surviving user turn that established them.
    if replay_state.previous_turn_settings.is_none() && active_segment.counts_as_user_turn {
        replay_state.previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving user turn baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(
        replay_state.reference_context_item,
        TurnReferenceContextItem::NeverSet
    ) && (active_segment.counts_as_user_turn
        || active_segment.base_compacted_item.is_some()
        || matches!(
            active_segment.reference_context_item,
            TurnReferenceContextItem::Cleared
        ))
    {
        replay_state.reference_context_item = active_segment.reference_context_item;
    }
}

fn establishes_world_state_boundary(item: &RolloutItem) -> bool {
    matches!(
        item,
        RolloutItem::Compacted(_) | RolloutItem::WorldState(WorldStateItem { full: true, .. })
    )
}

impl Session {
    pub(super) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> RolloutReconstruction {
        // Replay metadata should already match the shape of the future lazy reverse loader, even
        // while history materialization still uses an eager bridge. Scan newest-to-oldest,
        // stopping once a surviving replacement-history checkpoint and the required resume metadata
        // are both known; then replay only the buffered surviving tail forward to preserve exact
        // history semantics.
        let has_legacy_compaction_without_window_number =
            rollout_items.iter().any(|item| {
                matches!(item, RolloutItem::Compacted(compacted) if compacted.window_number.is_none())
            });
        let initial_window = if has_legacy_compaction_without_window_number {
            None
        } else {
            rollout_items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(session_meta) => session_meta
                    .meta
                    .context_window
                    .as_ref()
                    .and_then(reconstructed_window_from_session_context_window),
                _ => None,
            })
        };
        // Rollback is "drop the newest N user turns". While scanning in reverse, that becomes
        // "skip the next N user-turn segments we finalize".
        let mut replay_state = ReverseReplayState::default();
        // Reverse replay accumulates rollout items into the newest in-progress turn segment until
        // we hit its matching `TurnStarted`, at which point the segment can be finalized.
        let mut active_segment: Option<ActiveReplaySegment<'_>> = None;

        for (index, item) in rollout_items.iter().enumerate().rev() {
            match item {
                RolloutItem::Compacted(compacted) if compacted.replacement_history_media_repair => {
                    // A required repair is normally appended before the rollback marker. If reverse
                    // replay has already seen that marker, the repair may describe a semantic
                    // checkpoint from the rejected turn and must not become the surviving base.
                    let repair_precedes_pending_rollback = replay_state.pending_rollback_turns > 0;
                    if repair_precedes_pending_rollback {
                        replay_state.skipped_compacted_items.push(compacted);
                    }
                    // Representation repair is appended outside a model turn. Finalize any newer
                    // turn before selecting it so an older rollback cannot consume the repair as
                    // part of the preceding user-turn segment.
                    if let Some(active_segment) = active_segment.take() {
                        if replay_state.pending_rollback_turns > 0
                            || active_segment.counts_as_user_turn
                        {
                            finalize_active_segment(active_segment, &mut replay_state);
                        } else {
                            // A repair's immediately following companion records are also
                            // out-of-band. Apply them without requiring a user-turn boundary.
                            replay_state.world_state_boundary_known |= active_segment
                                .world_state_replay
                                .iter()
                                .any(|item| establishes_world_state_boundary(item));
                            replay_state
                                .world_state_replay
                                .extend(active_segment.world_state_replay);
                            if replay_state.base_compacted_item.is_none()
                                && let Some(segment_base_compacted_item) =
                                    active_segment.base_compacted_item
                            {
                                replay_state.base_compacted_item =
                                    Some(segment_base_compacted_item);
                                replay_state.rollout_suffix = active_segment.rollout_suffix;
                            }
                            if replay_state.window.is_none() {
                                replay_state.window = active_segment.window;
                            }
                            if replay_state.previous_turn_settings.is_none() {
                                replay_state.previous_turn_settings =
                                    active_segment.previous_turn_settings;
                            }
                            if matches!(
                                replay_state.reference_context_item,
                                TurnReferenceContextItem::NeverSet
                            ) && !matches!(
                                active_segment.reference_context_item,
                                TurnReferenceContextItem::NeverSet
                            ) {
                                replay_state.reference_context_item =
                                    active_segment.reference_context_item;
                            }
                        }
                    }
                    if !repair_precedes_pending_rollback
                        && replay_state.pending_rollback_turns == 0
                        && replay_state.window.is_none()
                        && let Some(window_number) = compacted.window_number
                    {
                        replay_state.window = Some(ReconstructedWindow {
                            number: window_number,
                            first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
                            previous_id: compacted
                                .previous_window_id
                                .as_deref()
                                .and_then(parse_uuid_v7),
                            id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
                        });
                    }
                    if !repair_precedes_pending_rollback
                        && replay_state.base_compacted_item.is_none()
                        && compacted.replacement_history.is_some()
                    {
                        replay_state.base_compacted_item = Some(compacted);
                        replay_state.rollout_suffix = Some(&rollout_items[index + 1..]);
                    }
                }
                RolloutItem::Compacted(compacted) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                    active_segment.compacted_items.push(compacted);
                    if active_segment.window.is_none()
                        && let Some(window_number) = compacted.window_number
                    {
                        active_segment.window = Some(ReconstructedWindow {
                            number: window_number,
                            first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
                            previous_id: compacted
                                .previous_window_id
                                .as_deref()
                                .and_then(parse_uuid_v7),
                            id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
                        });
                    }
                    // Looking backward, compaction clears any older baseline unless a newer
                    // `TurnContextItem` in this same segment has already re-established it.
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                    }
                    if replay_state.base_compacted_item.is_none()
                        && active_segment.base_compacted_item.is_none()
                        && compacted.replacement_history.is_some()
                    {
                        active_segment.base_compacted_item = Some(compacted);
                        active_segment.rollout_suffix = Some(&rollout_items[index + 1..]);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    replay_state.pending_rollback_turns = replay_state
                        .pending_rollback_turns
                        .saturating_add(usize::try_from(rollback.num_turns).unwrap_or(usize::MAX));
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // Reverse replay often sees `TurnComplete` before any turn-scoped metadata.
                    // Capture the turn id early so later `TurnContext` / abort items can match it.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = Some(event.turn_id.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                    if let Some(active_segment) = active_segment.as_mut() {
                        if active_segment.turn_id.is_none()
                            && let Some(turn_id) = &event.turn_id
                        {
                            active_segment.turn_id = Some(turn_id.clone());
                        }
                    } else if let Some(turn_id) = &event.turn_id {
                        active_segment = Some(ActiveReplaySegment {
                            turn_id: Some(turn_id.clone()),
                            ..Default::default()
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::TurnContext(ctx) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // `TurnContextItem` can attach metadata to an existing segment, but only a
                    // real `UserMessage` event should make the segment count as a user turn.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = ctx.turn_id.clone();
                    }
                    if turn_ids_are_compatible(
                        active_segment.turn_id.as_deref(),
                        ctx.turn_id.as_deref(),
                    ) {
                        active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                            model: ctx.model.clone(),
                            comp_hash: ctx.comp_hash.clone(),
                            realtime_active: ctx.realtime_active,
                        });
                        if matches!(
                            active_segment.reference_context_item,
                            TurnReferenceContextItem::NeverSet
                        ) {
                            active_segment.reference_context_item =
                                TurnReferenceContextItem::Latest(Box::new(ctx.clone()));
                        }
                    }
                }
                RolloutItem::WorldState(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                    // `TurnStarted` is the oldest boundary of the active reverse segment.
                    if active_segment.as_ref().is_some_and(|active_segment| {
                        turn_ids_are_compatible(
                            active_segment.turn_id.as_deref(),
                            Some(event.turn_id.as_str()),
                        )
                    }) && let Some(active_segment) = active_segment.take()
                    {
                        finalize_active_segment(active_segment, &mut replay_state);
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn |= is_user_turn_boundary(response_item);
                }
                RolloutItem::InterAgentCommunication(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
            }

            if replay_state.base_compacted_item.is_some()
                && replay_state.previous_turn_settings.is_some()
                && !matches!(
                    replay_state.reference_context_item,
                    TurnReferenceContextItem::NeverSet
                )
                && replay_state.pending_rollback_turns == 0
                && replay_state.world_state_boundary_known
            {
                // At this point we have the replacement-history base, eager resume metadata, and
                // a surviving world-state boundary, so older rollout items cannot affect this
                // result.
                break;
            }
        }

        if let Some(active_segment) = active_segment.take() {
            finalize_active_segment(active_segment, &mut replay_state);
        }
        let ReverseReplayState {
            base_compacted_item,
            previous_turn_settings,
            reference_context_item,
            mut world_state_replay,
            window,
            rollout_suffix,
            skipped_compacted_items,
            ..
        } = replay_state;
        let rollout_suffix = rollout_suffix.unwrap_or(rollout_items);

        let fallback_window_number = u64::try_from(
            rollout_items
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        RolloutItem::Compacted(compacted)
                            if !compacted.replacement_history_media_repair
                    )
                })
                .count(),
        )
        .unwrap_or(u64::MAX);

        let mut history = ContextManager::new();
        let mut saw_legacy_compaction_without_replacement_history = false;
        let mut repair_checkpoint_source = None;
        let mut repair_sanitization = crate::context::CompactedMediaSanitization::default();
        let mut repaired_prefix_len = 0usize;
        if let Some(base_compacted_item) = base_compacted_item
            && let Some(base_replacement_history) = &base_compacted_item.replacement_history
        {
            let mut base_replacement_history = base_replacement_history.clone();
            let prefix_len = if let Some(prefix_len) =
                base_compacted_item.replacement_history_media_sanitized_prefix_len
            {
                usize::try_from(prefix_len)
                    .unwrap_or(usize::MAX)
                    .min(base_replacement_history.len())
            } else {
                base_replacement_history.len()
            };
            repair_checkpoint_source = Some(base_compacted_item.clone());
            repaired_prefix_len = prefix_len;
            let sanitization = crate::context::sanitize_compacted_media_prefix(
                base_replacement_history.as_mut_slice(),
                prefix_len,
            );
            repair_sanitization.accumulate(sanitization);
            history.replace(base_replacement_history);
        }
        // Materialize exact history semantics from the replay-derived suffix. The eventual lazy
        // design should keep this same replay shape, but drive it from a resumable reverse source
        // instead of an eagerly loaded `&[RolloutItem]`.
        for item in rollout_suffix {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(
                        std::iter::once(&response_item),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunicationMetadata { .. } => {}
                RolloutItem::Compacted(compacted) => {
                    if skipped_compacted_items
                        .iter()
                        .any(|skipped| std::ptr::eq(*skipped, compacted))
                    {
                        continue;
                    }
                    if let Some(replacement_history) = &compacted.replacement_history {
                        repaired_prefix_len = compacted
                            .replacement_history_media_sanitized_prefix_len
                            .map(|prefix_len| usize::try_from(prefix_len).unwrap_or(usize::MAX))
                            .unwrap_or(replacement_history.len())
                            .min(replacement_history.len());
                        history.replace(replacement_history.clone());
                    } else {
                        saw_legacy_compaction_without_replacement_history = true;
                        // Legacy rollouts without `replacement_history` should rebuild the
                        // historical TurnContext at the correct insertion point from persisted
                        // `TurnContextItem`s. These are rare enough that we currently just clear
                        // `reference_context_item`, reinject canonical context at the end of the
                        // resumed conversation, and accept the temporary out-of-distribution
                        // prompt shape.
                        // TODO(ccunningham): if we drop support for None replacement_history compaction items,
                        // we can get rid of this second loop entirely and just build `history` directly in the first loop.
                        let user_messages = compact::collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                    repaired_prefix_len = repaired_prefix_len.min(history.raw_items().len());
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::SessionMeta(_) => {}
            }
        }

        let reference_context_item = match reference_context_item {
            TurnReferenceContextItem::NeverSet | TurnReferenceContextItem::Cleared => None,
            TurnReferenceContextItem::Latest(turn_reference_context_item) => {
                Some(*turn_reference_context_item)
            }
        };
        let reference_context_item = if saw_legacy_compaction_without_replacement_history {
            None
        } else {
            reference_context_item
        };

        // Segments and their contents were collected newest-first; replay the surviving records
        // chronologically so compaction resets and merge patches have their original meaning.
        world_state_replay.reverse();
        let mut world_state_baseline: Option<WorldStateSnapshot> = None;
        for item in world_state_replay {
            match item {
                RolloutItem::Compacted(_) => world_state_baseline = None,
                RolloutItem::WorldState(world_state) if world_state.full => {
                    world_state_baseline = match serde_json::from_value(world_state.state.clone()) {
                        Ok(snapshot) => Some(snapshot),
                        Err(err) => {
                            tracing::warn!(%err, "failed to restore world-state snapshot");
                            None
                        }
                    };
                }
                RolloutItem::WorldState(world_state) => {
                    let Some(baseline) = world_state_baseline.as_mut() else {
                        tracing::warn!("ignored world-state patch without a full snapshot");
                        continue;
                    };
                    if let Err(err) = baseline.apply_merge_patch(&world_state.state) {
                        tracing::warn!(%err, "failed to apply world-state patch");
                        world_state_baseline = None;
                    }
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::TurnContext(_)
                | RolloutItem::EventMsg(_) => {
                    unreachable!("only world-state replay items are collected")
                }
            }
        }

        let window = window.or(initial_window).unwrap_or(ReconstructedWindow {
            number: fallback_window_number,
            first_id: None,
            previous_id: None,
            id: None,
        });
        let mut history = history.into_raw_items();
        if repair_checkpoint_source.is_some() {
            let replay_sanitization = crate::context::sanitize_compacted_media_prefix(
                history.as_mut_slice(),
                repaired_prefix_len,
            );
            repair_sanitization.accumulate(replay_sanitization);
        }
        let needs_media_policy_certification =
            base_compacted_item.is_some_and(|base_compacted_item| {
                base_compacted_item.replacement_history.is_some()
                    && base_compacted_item
                        .replacement_history_media_sanitized_prefix_len
                        .is_none()
            });
        // A changed history invalidates every recorded usage snapshot. An already-marked
        // checkpoint needs a local estimate when no later server TokenCount can be restored, or
        // when a later model output, compaction, or rollback changed the reconstructed history it
        // described.
        let selected_checkpoint_has_valid_subsequent_token_info =
            base_compacted_item.is_some_and(|base_compacted_item| {
                rollout_items
                    .iter()
                    .position(|item| {
                        matches!(
                            item,
                            RolloutItem::Compacted(compacted)
                                if std::ptr::eq(compacted, base_compacted_item)
                        )
                    })
                    .is_some_and(|checkpoint_index| {
                        let after_checkpoint_index = checkpoint_index.saturating_add(1);
                        rollout_items[after_checkpoint_index..]
                            .iter()
                            .rposition(|item| {
                                matches!(
                                    item,
                                    RolloutItem::EventMsg(EventMsg::TokenCount(event))
                                        if event.info.is_some()
                                )
                            })
                            .map(|relative_token_index| {
                                after_checkpoint_index.saturating_add(relative_token_index)
                            })
                            .is_some_and(|token_index| {
                                !rollout_items[token_index.saturating_add(1)..]
                                    .iter()
                                    .any(|item| {
                                        matches!(
                                            item,
                                            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))
                                                | RolloutItem::Compacted(_)
                                        ) || matches!(
                                            item,
                                            RolloutItem::ResponseItem(response_item)
                                                if is_model_generated_item(response_item)
                                        )
                                    })
                            })
                    })
            });
        let selected_checkpoint_needs_token_recompute =
            base_compacted_item.is_some_and(|base_compacted_item| {
                base_compacted_item
                    .replacement_history_media_sanitized_prefix_len
                    .is_some()
                    && !selected_checkpoint_has_valid_subsequent_token_info
            });
        // User/tool suffix items after the latest server count are added by active-context
        // accounting. Rollback is different: it can remove items already included in that count,
        // so even a non-compacted history must replace the restored snapshot with a local estimate.
        let restored_token_info_invalidated_by_rollback = rollout_items
            .iter()
            .rposition(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(EventMsg::TokenCount(event)) if event.info.is_some()
                )
            })
            .is_some_and(|token_index| {
                rollout_items[token_index.saturating_add(1)..]
                    .iter()
                    .any(|item| {
                        matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)))
                    })
            });
        let should_recompute_token_usage = repair_sanitization.changed()
            || needs_media_policy_certification
            || selected_checkpoint_needs_token_recompute
            || restored_token_info_invalidated_by_rollback;
        let compacted_prefix_len = repair_checkpoint_source
            .as_ref()
            .map(|_| repaired_prefix_len);
        let repair = repair_checkpoint_source
            .filter(|_| repair_sanitization.changed() || needs_media_policy_certification)
            .map(|mut checkpoint| {
                checkpoint.replacement_history = Some(history.clone());
                checkpoint.window_number = Some(window.number);
                checkpoint.replacement_history_media_sanitized_prefix_len =
                    Some(u64::try_from(repaired_prefix_len).unwrap_or(u64::MAX));
                checkpoint.replacement_history_media_repair = true;
                RolloutReconstructionRepair {
                    checkpoint,
                    sanitization: repair_sanitization,
                    persistence: if repair_sanitization.changed() {
                        RolloutReconstructionRepairPersistence::Required
                    } else {
                        RolloutReconstructionRepairPersistence::BestEffort
                    },
                }
            });
        RolloutReconstruction {
            history,
            compacted_prefix_len,
            repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number: window.number,
            first_window_id: window.first_id,
            previous_window_id: window.previous_id,
            window_id: window.id,
        }
    }
}

fn parse_uuid_v7(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
}

fn reconstructed_window_from_session_context_window(
    context_window: &SessionContextWindow,
) -> Option<ReconstructedWindow> {
    let id = parse_uuid_v7(&context_window.window_id)?;
    Some(ReconstructedWindow {
        number: 0,
        first_id: Some(id),
        previous_id: None,
        id: Some(id),
    })
}
