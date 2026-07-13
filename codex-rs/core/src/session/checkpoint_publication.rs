//! Canonical checkpoint publication, separate from model-context selection.

use super::*;
use codex_extension_api::RestoredSkillsInventory;

impl Session {
    pub(super) async fn publish_compacted_history(
        &self,
        mut items: Vec<ResponseItemEnvelope>,
        reference_context_item: Option<TurnContextItem>,
        world_state_baseline: Option<Arc<WorldState>>,
        metadata: CompactedHistoryMetadata,
    ) -> CodexResult<Vec<ResponseItemEnvelope>> {
        // Goal-first ordering: never acquire a contributor lease while holding publication.
        let mut contributions = Vec::new();
        for contributor in self.services.extensions.context_contributors() {
            contributions.push(
                contributor
                    .contribute_post_compaction_context(
                        &self.services.session_extension_data,
                        &self.services.thread_extension_data,
                    )
                    .await,
            );
        }
        let permit = thread_settings::acquire_persistence_lock(self).await;
        self.check_history_publication()?;
        let state = self.state.lock().await;
        let source = state.history.annotated_items();
        let source_mcp = source
            .iter()
            .filter(|envelope| {
                crate::context::McpServerUseInstructions::matches_response_item(&envelope.item)
            })
            .cloned()
            .collect::<Vec<_>>();
        // The accepted source is authoritative, including equal text at distinct positions.
        // Retain any genuinely new replacement envelope as well, never deduplicate the source.
        let mut retained = source_mcp.clone();
        items.retain(|envelope| {
            if !crate::context::McpServerUseInstructions::matches_response_item(&envelope.item) {
                return true;
            }
            if !source_mcp.contains(envelope) {
                retained.push(envelope.clone());
            }
            false
        });
        let inventory = source
            .iter()
            .rev()
            .find_map(RestoredSkillsInventory::from_envelope)
            .or_else(|| {
                items
                    .iter()
                    .rev()
                    .find_map(RestoredSkillsInventory::from_envelope)
            });
        items.retain(|envelope| RestoredSkillsInventory::from_envelope(envelope).is_none());
        // Only newly constructed replacement items need identity. Accepted source
        // envelopes must survive byte-for-byte, including legacy absent IDs.
        for envelope in &mut items {
            Self::assign_missing_response_item_id(&mut envelope.item);
        }
        if let Some(inventory) = inventory {
            retained.push(inventory.envelope().clone());
        }
        for contribution in &mut contributions {
            retained.extend(
                contribution
                    .take_items()
                    .into_iter()
                    .map(ResponseItemEnvelope::new),
            );
        }
        let boundary = items
            .iter()
            .position(|envelope| {
                matches!(
                    &envelope.item,
                    ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                ) || matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user")
            })
            .unwrap_or(items.len());
        items.splice(boundary..boundary, retained);
        if let Some(checkpoint) = items.iter_mut().rev().find(|envelope| {
            matches!(
                envelope.item,
                ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
            )
        }) {
            checkpoint
                .metadata
                .get_or_insert_default()
                .compaction_model_hash = metadata.compaction_model_hash;
        }
        let mut projected = state.history.clone();
        projected.replace_compacted(items.clone(), metadata.reviewer_compaction_hash.as_deref());
        let compacted_item = CompactedItem {
            message: metadata.message,
            replacement_history: Some(items.clone()),
            retained_context: Some(projected.retained_context().clone()),
            guardian_history: projected.guardian_history_checkpoint(),
            mcp_resource_origins: self.services.mcp_runtime.resource_origin_checkpoint(),
            compaction_summary_tokens: metadata.compaction_summary_tokens,
            window_number: Some(metadata.window_number),
            first_window_id: Some(metadata.window_ids.first_window_id.to_string()),
            previous_window_id: metadata
                .window_ids
                .previous_window_id
                .map(|id| id.to_string()),
            window_id: Some(metadata.window_ids.window_id.to_string()),
            compaction_response_id: metadata.compaction_response_id,
            latest_token_usage_record: state.latest_token_usage_record.clone(),
        };
        drop(state);
        let mut rollout_items = vec![RolloutItem::Compacted(compacted_item)];
        let world_state_snapshot = world_state_baseline.map(|world_state| world_state.snapshot());
        if let Some(snapshot) = &world_state_snapshot {
            rollout_items.push(RolloutItem::WorldState(WorldStateItem::full(
                snapshot.clone().into_object(),
            )));
        }
        if let Some(context) = &reference_context_item {
            rollout_items.push(RolloutItem::TurnContext(context.clone()));
        }
        rollout_items.push(RolloutItem::EventMsg(
            thread_settings::applied_event(self).await,
        ));
        let receiver = self.dispatch_history_publication(
            permit,
            rollout_items,
            contributions,
            /*acknowledgement*/ None,
            move |state| {
                let installed = items.clone();
                state.replace_annotated_history(
                    items,
                    reference_context_item,
                    HistoryReplacement::Compaction {
                        reviewer_compaction_hash: metadata.reviewer_compaction_hash,
                    },
                );
                state.reasoning_effort_pin = ReasoningEffortPin::Compacted;
                if let Some(snapshot) = world_state_snapshot {
                    state.history.set_world_state_baseline(snapshot);
                }
                state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
                installed
            },
        )?;
        self.publication_result(receiver).await
    }
}
