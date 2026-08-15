use super::*;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::is_model_generated_item;
use crate::context_manager::is_user_turn_boundary;
use codex_history::ResponseItemEnvelope;
use codex_history::exact_rollback_removed_items;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::ThreadHistoryMode;
use std::collections::HashSet;
use uuid::Uuid;

// Return value of `Session::reconstruct_history_from_rollout`, bundling the rebuilt history with
// the resume/fork hydration metadata derived from the same replay.
#[derive(Debug, PartialEq)]
pub(super) struct RolloutReconstruction {
    pub(super) history: Vec<ResponseItemEnvelope>,
    pub(super) retained_context: codex_history::RetainedContext,
    pub(super) guardian_history: Option<codex_history::GuardianHistoryCheckpoint>,
    pub(super) last_started_turn_id: Option<String>,
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

#[derive(Debug, PartialEq)]
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
    pub(super) last_started_turn_id: Option<String>,
    pub(super) history: Vec<ResponseItemEnvelope>,
    pub(super) retained_context: codex_history::RetainedContext,
    pub(super) guardian_history: Option<codex_history::GuardianHistoryCheckpoint>,
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

#[derive(Debug, Clone, Copy)]
// The selected compaction and its replay tail must belong to the same surviving segment.
struct ReplayCheckpoint<'a> {
    compacted: &'a CompactedItem,
    suffix: &'a [RolloutItem],
}

/// Selects the newest compaction that can safely bound replay.
///
/// Returns `None` when reconstruction must replay all supplied items, either because there is no
/// compaction or the newest compaction cannot bound replay.
fn select_input_compaction<'a>(
    rollout_items: &'a [RolloutItem],
    exact_rollback_removals: &[bool],
    history_mode: ThreadHistoryMode,
) -> Option<ReplayCheckpoint<'a>> {
    // Only the newest compaction can bound replay. If it is incomplete, an older compaction
    // cannot replace the history or window state that the newer one may have changed.
    let (index, compacted) = rollout_items
        .iter()
        .enumerate()
        .rev()
        .filter(|(index, _)| !exact_rollback_removals[*index])
        .find_map(|(index, item)| match item {
            RolloutItem::Compacted(compacted) => Some((index, compacted)),
            _ => None,
        })?;
    // Paginated histories always honor this boundary. Other histories only do so when resume
    // metadata identifies a compaction written under the newer resume contract.
    if compacted.replacement_history_media_repair
        || compacted.replacement_history.is_none()
        || compacted.window_number.is_none()
        || (compacted.resume_metadata.is_none()
            && !matches!(history_mode, ThreadHistoryMode::Paginated))
    {
        return None;
    }
    // A Legacy count-only marker can remove turns on the older side of this checkpoint.
    // Resolve those historical boundaries through full replay rather than seeding a base
    // that the reverse segment scan has not yet proved to survive. Paginated history may
    // contain only this window: retain its independent Guardian checkpoint and let forward
    // replay apply the rollback to the replacement history and its accepted-input evidence.
    if matches!(history_mode, ThreadHistoryMode::Legacy)
        && rollout_items[index + 1..]
            .iter()
            .enumerate()
            .any(|(offset, item)| {
                !exact_rollback_removals[index + 1 + offset]
                    && matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback))
                    if rollback.rollback_start_index.is_none() && rollback.num_turns > 0)
            })
    {
        return None;
    }
    Some(ReplayCheckpoint {
        compacted,
        suffix: &rollout_items[index + 1..],
    })
}

#[derive(Debug, Default)]
struct ActiveReplaySegment<'a> {
    turn_id: Option<String>,
    turn_completed: bool,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    compacted_items: Vec<&'a CompactedItem>,
    history_checkpoint: Option<ReplayCheckpoint<'a>>,
    window: Option<ReconstructedWindow>,
}

#[derive(Debug, Default)]
struct ReverseReplayState<'a> {
    history_checkpoint: Option<ReplayCheckpoint<'a>>,
    metadata_checkpoint: Option<&'a CompactedItem>,
    repair_companion_previous_turn_settings: Option<PreviousTurnSettings>,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    window: Option<ReconstructedWindow>,
    pending_rollback_turns: usize,
    skipped_compacted_items: Vec<&'a CompactedItem>,
    skipped_turn_start_indices: HashSet<usize>,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn finalize_active_segment<'a>(
    active_segment: ActiveReplaySegment<'a>,
    replay_state: &mut ReverseReplayState<'a>,
) {
    // Legacy thread rollback markers count user-turn segments.
    if replay_state.pending_rollback_turns > 0 {
        replay_state
            .skipped_compacted_items
            .extend(active_segment.compacted_items);
        if active_segment.counts_as_user_turn {
            replay_state.pending_rollback_turns -= 1;
        }
        return;
    }

    // Full world-state snapshots are persisted after installing initial context. They still
    // establish a baseline when a child fork removes the parent turn's agent message. Do not
    // count these context-only segments as user turns for rollback, or use a snapshot from
    // before the segment's latest compaction.
    let has_context_baseline = active_segment.counts_as_user_turn
        || active_segment
            .world_state_replay
            .iter()
            .take_while(|item| !matches!(item, RolloutItem::Compacted(_)))
            .any(|item| matches!(item, RolloutItem::WorldState(state) if state.full));
    replay_state
        .world_state_replay
        .extend(active_segment.world_state_replay);

    // A surviving replacement-history compaction is a complete history base. Once we
    // know the newest surviving one, older rollout items do not affect rebuilt history.
    if replay_state.history_checkpoint.is_none() {
        replay_state.history_checkpoint = active_segment.history_checkpoint;
    }
    if replay_state.metadata_checkpoint.is_none() {
        replay_state.metadata_checkpoint = active_segment
            .compacted_items
            .iter()
            .copied()
            .find(|checkpoint| checkpoint.resume_metadata.is_some());
    }

    if replay_state.window.is_none() {
        replay_state.window = active_segment.window;
    }

    // Restore settings from the newest surviving context baseline.
    if replay_state.previous_turn_settings.is_none() && has_context_baseline {
        replay_state.previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving context baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(
        replay_state.reference_context_item,
        TurnReferenceContextItem::NeverSet
    ) && (has_context_baseline
        || matches!(
            active_segment.reference_context_item,
            TurnReferenceContextItem::Cleared
        ))
    {
        replay_state.reference_context_item = active_segment.reference_context_item;
    }
}

impl Session {
    pub(super) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> RolloutReconstruction {
        // Retain the target's self-contained semantic-checkpoint fast path. A historical
        // representation repair needs its older semantic metadata, not an invented cutoff.
        let exact_rollback_removals = exact_rollback_removed_items(rollout_items);
        let input_checkpoint = select_input_compaction(
            rollout_items,
            &exact_rollback_removals,
            turn_context.history_mode,
        );
        let replay_items = input_checkpoint.map_or(rollout_items, |checkpoint| checkpoint.suffix);
        let replay_start = rollout_items.len().saturating_sub(replay_items.len());
        let session_initial_window = rollout_items.iter().find_map(|item| match item {
            RolloutItem::SessionMeta(session_meta) => session_meta
                .meta
                .context_window
                .as_ref()
                .and_then(reconstructed_window_from_session_context_window),
            _ => None,
        });
        // Rollback is "drop the newest N user turns". While scanning in reverse, that becomes
        // "skip the next N user-turn segments we finalize".
        let mut replay_state = ReverseReplayState {
            history_checkpoint: input_checkpoint,
            metadata_checkpoint: input_checkpoint
                .map(|checkpoint| checkpoint.compacted)
                .filter(|checkpoint| checkpoint.resume_metadata.is_some()),
            window: input_checkpoint
                .and_then(|checkpoint| reconstructed_window_from_compaction(checkpoint.compacted)),
            ..Default::default()
        };
        // Reverse replay accumulates rollout items into the newest in-progress turn segment until
        // we hit its matching `TurnStarted`, at which point the segment can be finalized.
        let mut active_segment: Option<ActiveReplaySegment<'_>> = None;
        let trusted_completions =
            completion_replay::trusted_contexts(rollout_items, &exact_rollback_removals);
        let mut deduplicated_completion_context = false;
        for (index, item) in replay_items.iter().enumerate().rev() {
            if exact_rollback_removals[replay_start + index] {
                if let RolloutItem::Compacted(compacted) = item {
                    replay_state.skipped_compacted_items.push(compacted);
                }
                continue;
            }
            match item {
                RolloutItem::Compacted(compacted) if compacted.replacement_history_media_repair => {
                    // Older writers could append a repair before the rollback marker. If reverse
                    // replay has already seen that marker, the repair describes a semantic
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
                            replay_state
                                .world_state_replay
                                .extend(active_segment.world_state_replay);
                            if replay_state.history_checkpoint.is_none() {
                                replay_state.history_checkpoint = active_segment.history_checkpoint;
                            }
                            if replay_state.window.is_none() {
                                replay_state.window = active_segment.window;
                            }
                            if replay_state
                                .repair_companion_previous_turn_settings
                                .is_none()
                            {
                                replay_state.repair_companion_previous_turn_settings =
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
                    if !repair_precedes_pending_rollback {
                        if replay_state.history_checkpoint.is_none()
                            && compacted.replacement_history.is_some()
                        {
                            replay_state.history_checkpoint = Some(ReplayCheckpoint {
                                compacted,
                                suffix: &replay_items[index + 1..],
                            });
                        }
                        if replay_state.metadata_checkpoint.is_none()
                            && compacted.resume_metadata.is_some()
                        {
                            replay_state.metadata_checkpoint = Some(compacted);
                        }
                    }
                }
                RolloutItem::Compacted(compacted) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                    active_segment.compacted_items.push(compacted);
                    if active_segment.window.is_none()
                        && let Some(compaction_window) =
                            reconstructed_window_from_compaction(compacted)
                    {
                        active_segment.window = Some(compaction_window);
                    }
                    // Looking backward, compaction clears any older baseline unless a newer
                    // `TurnContextItem` in this same segment has already re-established it.
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                    }
                    if compacted.resume_metadata.is_some() && !active_segment.turn_completed {
                        active_segment.previous_turn_settings = None;
                    }
                    if replay_state.history_checkpoint.is_none()
                        && active_segment.history_checkpoint.is_none()
                        && compacted.replacement_history.is_some()
                    {
                        active_segment.history_checkpoint = Some(ReplayCheckpoint {
                            compacted,
                            suffix: &replay_items[index + 1..],
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    if rollback.rollback_start_index.is_none() && rollback.num_turns > 0 {
                        // A checkpoint or other context appended after this marker
                        // describes the surviving state, not the turn it removed.
                        if let Some(segment) = active_segment.take() {
                            finalize_active_segment(segment, &mut replay_state);
                        }
                        replay_state.pending_rollback_turns =
                            replay_state.pending_rollback_turns.saturating_add(
                                usize::try_from(rollback.num_turns).unwrap_or(usize::MAX),
                            );
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.turn_completed = true;
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
                    ) && !active_segment
                        .compacted_items
                        .iter()
                        .any(|checkpoint| checkpoint.resume_metadata.is_some())
                    {
                        active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                            model: ctx.model.clone(),
                            cyber_access_program: ctx.cyber_access_program,
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
                        if replay_state.pending_rollback_turns > 0 {
                            replay_state
                                .skipped_turn_start_indices
                                .insert(replay_start + index);
                        }
                        finalize_active_segment(active_segment, &mut replay_state);
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn |=
                        is_user_turn_boundary(&response_item.item);
                }
                RolloutItem::InterAgentCommunication(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::AgentResponseObservation(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::RetainedContext(_)
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
            }
        }

        if let Some(mut active_segment) = active_segment.take() {
            // Frozen companion context is a baseline, not a completed turn's settings.
            if replay_state.metadata_checkpoint.is_some() && !active_segment.turn_completed {
                active_segment.previous_turn_settings = None;
            }
            finalize_active_segment(active_segment, &mut replay_state);
        }
        let ReverseReplayState {
            history_checkpoint,
            metadata_checkpoint,
            repair_companion_previous_turn_settings,
            mut previous_turn_settings,
            reference_context_item,
            mut world_state_replay,
            window,
            skipped_compacted_items,
            skipped_turn_start_indices,
            ..
        } = replay_state;
        let base_compacted_item = history_checkpoint.map(|checkpoint| checkpoint.compacted);
        let rollout_suffix =
            history_checkpoint.map_or(rollout_items, |checkpoint| checkpoint.suffix);
        let rollout_suffix_start = rollout_items.len().saturating_sub(rollout_suffix.len());
        let resume_metadata =
            metadata_checkpoint.and_then(|checkpoint| checkpoint.resume_metadata.as_ref());
        let last_started_turn_items = metadata_checkpoint
            .and_then(|checkpoint| {
                rollout_items.iter().position(|item| {
                matches!(item, RolloutItem::Compacted(item) if std::ptr::eq(item, checkpoint))
            }).map(|index| &rollout_items[index + 1..])
            })
            .unwrap_or(replay_items);
        let last_started_turn_start = rollout_items
            .len()
            .saturating_sub(last_started_turn_items.len());
        let last_started_turn_id = last_started_turn_items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(offset, item)| {
                if exact_rollback_removals[last_started_turn_start + offset]
                    || skipped_turn_start_indices.contains(&(last_started_turn_start + offset))
                {
                    return None;
                }
                match item {
                    RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                        Some(event.turn_id.clone())
                    }
                    _ => None,
                }
            })
            .or_else(|| resume_metadata.and_then(|metadata| metadata.last_started_turn_id.clone()));
        let was_skipped = |compacted: &CompactedItem| {
            skipped_compacted_items
                .iter()
                .any(|skipped| std::ptr::eq(*skipped, compacted))
        };
        let has_legacy_compaction_without_window_number =
            rollout_items.iter().enumerate().any(|(index, item)| {
                !exact_rollback_removals[index]
                    && matches!(
                        item,
                        RolloutItem::Compacted(compacted)
                            if compacted.window_number.is_none()
                                && !compacted.replacement_history_media_repair
                                && !was_skipped(compacted)
                    )
            });
        let initial_window = if has_legacy_compaction_without_window_number {
            None
        } else {
            session_initial_window
        };

        if previous_turn_settings.is_none() {
            previous_turn_settings = match resume_metadata {
                Some(metadata) => metadata.previous_turn_settings.clone(),
                None => repair_companion_previous_turn_settings,
            };
        }

        let fallback_window_number = u64::try_from(
            rollout_items
                .iter()
                .enumerate()
                .filter(|(index, item)| {
                    !exact_rollback_removals[*index]
                        && matches!(
                            item,
                        RolloutItem::Compacted(compacted)
                            if !compacted.replacement_history_media_repair
                                && !was_skipped(compacted)
                        )
                })
                .count(),
        )
        .unwrap_or(u64::MAX);

        // Build model-visible history from the selected compaction and its newer suffix.
        let mut history = ContextManager::for_session(
            &turn_context.session_source,
            &turn_context.config.features,
        );
        let mut saw_legacy_compaction_without_replacement_history = false;
        let mut repair_checkpoint_source = None;
        let mut repair_sanitization = crate::context::CompactedMediaSanitization::default();
        let mut repaired_prefix_len = 0usize;
        if let Some(base_compacted_item) = base_compacted_item
            && let Some(base_replacement_history) = &base_compacted_item.replacement_history
        {
            let mut base_replacement_history = base_replacement_history.clone();
            completion_replay::normalize_unproven_tasks(
                &mut base_replacement_history,
                &trusted_completions,
            );
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
            let (prefix_len, deduplicated) = completion_replay::deduplicate(
                &mut base_replacement_history,
                prefix_len,
                &trusted_completions,
            );
            repaired_prefix_len = prefix_len;
            deduplicated_completion_context |= deduplicated;
            let sanitization = crate::context::sanitize_compacted_media_envelopes(
                base_replacement_history.as_mut_slice(),
                repaired_prefix_len,
            );
            repair_sanitization.accumulate(sanitization);
            let mut guardian_checkpoint = base_compacted_item.guardian_history.clone();
            // An old unmarked checkpoint is entirely summarized. Marked repair/fork
            // snapshots may carry current-window Guardian evidence after their prefix.
            if (base_compacted_item
                .replacement_history_media_sanitized_prefix_len
                .is_none()
                || (!base_compacted_item.replacement_history_media_repair
                    && base_compacted_item.resume_metadata.is_some()
                    && prefix_len == base_replacement_history.len()))
                && let Some(checkpoint) = &mut guardian_checkpoint
            {
                let guardian_prefix = checkpoint.0.len();
                repair_sanitization.accumulate(crate::context::sanitize_compacted_media_envelopes(
                    &mut checkpoint.0,
                    guardian_prefix,
                ));
            }
            history.replace_annotated(base_replacement_history);
            history.restore_review_context(
                base_compacted_item.retained_context.as_ref(),
                guardian_checkpoint.as_ref(),
                /*reviewer_compaction_hash*/ None,
            );
        }
        let mut completion_context_ids =
            completion_replay::context_ids(history.annotated_items(), &trusted_completions);
        // Materialize exact history semantics from the replay-derived suffix. The eventual lazy
        // design should keep this same replay shape, but drive it from a resumable reverse source
        // instead of an eagerly loaded `&[RolloutItem]`.
        for (offset, item) in rollout_suffix.iter().enumerate() {
            let index = rollout_suffix_start.saturating_add(offset);
            if exact_rollback_removals[index] {
                continue;
            }
            match item {
                RolloutItem::RetainedContext(event) => {
                    history.record_retained_context(event);
                }
                RolloutItem::ResponseItem(response_item) => {
                    if matches!(
                        index
                            .checked_sub(1)
                            .filter(|previous| !exact_rollback_removals[*previous])
                            .and_then(|index| rollout_items.get(index)),
                        Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                    ) && response_item.id().is_some_and(|id| {
                        trusted_completions.get(id) == Some(response_item)
                            && !completion_context_ids.insert(id.clone())
                    }) {
                        deduplicated_completion_context = true;
                        continue;
                    }
                    let mut response_item = response_item.clone();
                    completion_replay::normalize_unproven_tasks(
                        std::slice::from_mut(&mut response_item),
                        &trusted_completions,
                    );
                    history.replay_annotated_item(
                        &response_item,
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(
                        std::iter::once(&response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::AgentResponseObservation(_) => {}
                RolloutItem::Compacted(compacted) => {
                    if skipped_compacted_items
                        .iter()
                        .any(|skipped| std::ptr::eq(*skipped, compacted))
                    {
                        continue;
                    }
                    if let Some(replacement_history) = &compacted.replacement_history {
                        // This should actually never happen, because the reverse loop above (to build rollout_suffix)
                        // should stop before any compaction that has Some replacement_history
                        let prefix_len = compacted
                            .replacement_history_media_sanitized_prefix_len
                            .map(|prefix_len| usize::try_from(prefix_len).unwrap_or(usize::MAX))
                            .unwrap_or(replacement_history.len())
                            .min(replacement_history.len());
                        let mut replacement_history = replacement_history.clone();
                        completion_replay::normalize_unproven_tasks(
                            &mut replacement_history,
                            &trusted_completions,
                        );
                        let (prefix_len, deduplicated) = completion_replay::deduplicate(
                            &mut replacement_history,
                            prefix_len,
                            &trusted_completions,
                        );
                        repaired_prefix_len = prefix_len;
                        deduplicated_completion_context |= deduplicated;
                        history.replace_annotated(replacement_history);
                        completion_context_ids = completion_replay::context_ids(
                            history.annotated_items(),
                            &trusted_completions,
                        );
                        history.restore_review_context(
                            compacted.retained_context.as_ref(),
                            compacted.guardian_history.as_ref(),
                            /*reviewer_compaction_hash*/ None,
                        );
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
                        let user_messages =
                            compact::collect_annotated_user_messages(history.annotated_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        let retained_context = history.retained_context().clone();
                        repaired_prefix_len = rebuilt.len();
                        history.replace_annotated(rebuilt);
                        completion_context_ids = completion_replay::context_ids(
                            history.annotated_items(),
                            &trusted_completions,
                        );
                        history.restore_retained_context(Some(&retained_context));
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    if rollback.rollback_start_index.is_none() {
                        history.drop_last_n_user_turns(rollback.num_turns);
                        repaired_prefix_len =
                            repaired_prefix_len.min(history.annotated_items().len());
                        completion_context_ids = completion_replay::context_ids(
                            history.annotated_items(),
                            &trusted_completions,
                        );
                    }
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::TokenUsageRecord(_)
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

        // Replay the collected world-state records chronologically so compaction resets and merge
        // patches keep their original meaning.
        world_state_replay.reverse();
        let mut world_state_baseline: Option<WorldStateSnapshot> = None;
        for item in world_state_replay {
            match item {
                RolloutItem::Compacted(_) => world_state_baseline = None,
                RolloutItem::WorldState(world_state) if world_state.full => {
                    world_state_baseline = Some(WorldStateSnapshot::from(&world_state.state));
                }
                RolloutItem::WorldState(world_state) => {
                    let Some(baseline) = world_state_baseline.as_mut() else {
                        tracing::warn!("ignored world-state patch without a full snapshot");
                        continue;
                    };
                    baseline.apply_merge_patch(&world_state.state);
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::AgentResponseObservation(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::RetainedContext(_)
                | RolloutItem::SecurityRiskScore(_)
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
        let retained_context = history.retained_context().clone();
        let guardian_history = history.guardian_history_checkpoint();
        let mut history = history.into_annotated_items();
        if repair_checkpoint_source.is_some() {
            let replay_sanitization = crate::context::sanitize_compacted_media_envelopes(
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
                                                if is_model_generated_item(&response_item.item)
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
            || deduplicated_completion_context
            // A canonical checkpoint may retain acknowledged mailbox context that was still
            // absent from the live model request associated with a later server TokenCount.
            // Recompute conservatively without treating a reserved ID as trusted provenance.
            || base_compacted_item.is_some_and(|checkpoint| {
                checkpoint.replacement_history.as_ref().is_some_and(|items| {
                    items.iter().filter_map(|item| item.id()).any(|id| {
                        codex_protocol::protocol::is_sub_agent_completion_context_response_item_id(id.as_str())
                    })
                })
            })
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
                checkpoint.retained_context = Some(retained_context.clone());
                checkpoint.guardian_history = guardian_history.clone();
                checkpoint.latest_token_usage_record =
                    Self::last_token_usage_record_from_rollout(rollout_items);
                checkpoint.window_number = Some(window.number);
                checkpoint.resume_metadata = Some(CompactionResumeMetadata {
                    multi_agent_version: self.multi_agent_version(),
                    last_started_turn_id: last_started_turn_id.clone(),
                    previous_turn_settings: previous_turn_settings.clone(),
                });
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
            last_started_turn_id,
            retained_context,
            history,
            guardian_history,
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

#[cfg(test)]
#[path = "rollout_rollback_checkpoint_tests.rs"]
mod rollback_checkpoint_tests;
fn parse_uuid_v7(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
}

fn reconstructed_window_from_compaction(compacted: &CompactedItem) -> Option<ReconstructedWindow> {
    Some(ReconstructedWindow {
        number: compacted.window_number?,
        first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
        previous_id: compacted
            .previous_window_id
            .as_deref()
            .and_then(parse_uuid_v7),
        id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
    })
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
