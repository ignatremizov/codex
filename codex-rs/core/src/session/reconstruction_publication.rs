//! Prepares replay repairs and publishes their canonical checkpoint before live state.

use super::*;

impl Session {
    pub(super) async fn apply_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> CodexResult<rollout_reconstruction::AppliedRolloutReconstruction> {
        let reconstruction = self
            .prepare_rollout_reconstruction(turn_context, rollout_items)
            .await;
        self.install_rollout_reconstruction(turn_context, reconstruction, Vec::new())
            .await
    }

    #[instrument(
        level = "trace",
        skip_all,
        fields(
            thread_id = %self.thread_id(),
            rollout_item_count = rollout_items.len()
        )
    )]
    pub(super) async fn prepare_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> rollout_reconstruction::PreparedRolloutReconstruction {
        let rollout_reconstruction::RolloutReconstruction {
            mut history,
            retained_context,
            guardian_history,
            compacted_prefix_len,
            mut repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = self
            .reconstruct_history_from_rollout(turn_context, rollout_items)
            .await;
        let fallback_ids = {
            let state = self.state.lock().await;
            state.auto_compact_window_ids()
        };
        let effective_window_id = window_id.unwrap_or(fallback_ids.window_id);
        let effective_first_window_id = first_window_id.unwrap_or(effective_window_id);
        if let Some(repair) = repair.as_mut() {
            repair.checkpoint.window_number = Some(window_number);
            repair.checkpoint.first_window_id = Some(effective_first_window_id.to_string());
            repair.checkpoint.previous_window_id = previous_window_id.map(|id| id.to_string());
            repair.checkpoint.window_id = Some(effective_window_id.to_string());
        }
        let repair = repair.map(|repair| {
            let mut repair_items = vec![RolloutItem::Compacted(repair.checkpoint)];
            if let Some(world_state_baseline) = world_state_baseline.as_ref() {
                repair_items.push(RolloutItem::WorldState(WorldStateItem::full(
                    world_state_baseline.clone().into_object(),
                )));
            }
            if let Some(reference_context_item) = reference_context_item.as_ref() {
                repair_items.push(RolloutItem::TurnContext(reference_context_item.clone()));
            }
            rollout_reconstruction::AppliedRolloutReconstructionRepair {
                items: repair_items,
                sanitization: repair.sanitization,
                persistence: repair.persistence,
            }
        });
        // Prepare unsummarized suffix media before installing reconstructed history. Historic
        // compacted media has already been replaced with bounded references; its repair checkpoint
        // is persisted separately without rewriting the source records on this critical path.
        // Never backfill resize notices during replay; only newly recorded items may emit them,
        // so the historical model prefix remains unchanged.
        let (mut prepared_history, metadata): (Vec<_>, Vec<_>) = history
            .into_iter()
            .map(|envelope| (envelope.item, envelope.metadata))
            .unzip();
        // Replay must not upload or migrate recorded history. The inline store returns prepared
        // inline bytes, while existing file references bypass preparation and remain unchanged.
        // Bound replay future size now that image preparation can await storage.
        let _ = Box::pin(prepare_image_response_items(
            &mut prepared_history,
            ImagePreparationMode::DetailBased,
            ImageResizeNoticeMode::Disabled,
            &InlineAttachmentStore,
        ))
        .await;
        prepare_audio_response_items(&mut prepared_history);
        assert_eq!(
            prepared_history.len(),
            metadata.len(),
            "replay media preparation must remain one-to-one when resize notices are disabled"
        );
        history = prepared_history
            .into_iter()
            .zip(metadata)
            .map(|(item, metadata)| ResponseItemEnvelope { item, metadata })
            .collect();
        rollout_reconstruction::PreparedRolloutReconstruction {
            history,
            retained_context,
            guardian_history,
            compacted_prefix_len,
            repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number,
            first_window_id: effective_first_window_id,
            previous_window_id,
            window_id: effective_window_id,
        }
    }

    pub(super) async fn install_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        reconstruction: rollout_reconstruction::PreparedRolloutReconstruction,
        mut prefix: Vec<RolloutItem>,
    ) -> CodexResult<rollout_reconstruction::AppliedRolloutReconstruction> {
        let rollout_reconstruction::PreparedRolloutReconstruction {
            history,
            retained_context,
            guardian_history,
            compacted_prefix_len,
            mut repair,
            should_recompute_token_usage,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = reconstruction;
        let reviewer_compaction_hash =
            if self.guardian_context_mode == crate::context::GuardianContextMode::ThreadOwned {
                let context = crate::guardian::GuardianReviewContext::from(turn_context);
                let (_, reviewer) = crate::guardian::resolve_review_model(self, &context).await;
                reviewer.comp_hash.clone()
            } else {
                None
            };
        if repair.as_ref().is_some_and(|repair| {
            matches!(
                repair.persistence,
                rollout_reconstruction::RolloutReconstructionRepairPersistence::Required
            )
        }) && let Some(required) = repair.take()
        {
            prefix.extend(required.items);
        }
        let permit = thread_settings::acquire_persistence_lock(self).await;
        let applied = rollout_reconstruction::AppliedRolloutReconstruction {
            previous_turn_settings: previous_turn_settings.clone(),
            repair,
            should_recompute_token_usage,
        };
        let receiver = self.dispatch_history_publication(
            permit,
            prefix,
            Vec::new(),
            /*acknowledgement*/ None,
            move |state| {
                state.replace_annotated_history(
                    history,
                    reference_context_item,
                    HistoryReplacement::Reset,
                );
                state.history.restore_review_context(
                    Some(&retained_context),
                    guardian_history.as_ref(),
                    reviewer_compaction_hash.as_deref(),
                );
                state.history.set_compacted_prefix_len(compacted_prefix_len);
                if let Some(world_state) = world_state_baseline {
                    state.history.set_world_state_baseline(world_state);
                }
                state.restore_auto_compact_window(
                    window_number,
                    AutoCompactWindowIds {
                        first_window_id,
                        previous_window_id,
                        window_id,
                    },
                );
                state.set_previous_turn_settings(previous_turn_settings.clone());
                applied
            },
        )?;
        let applied = self.publication_result(receiver).await?;
        let prefix_tokens = if matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            let history = self.clone_history().await;
            let base_instructions = self.get_base_instructions().await;
            history.estimate_token_count_with_base_instructions(&base_instructions)
        } else {
            None
        };
        if let Some(prefix_tokens) = prefix_tokens {
            self.set_auto_compact_window_estimated_prefill_for_scope(turn_context, prefix_tokens)
                .await;
        }
        Ok(applied)
    }
}
