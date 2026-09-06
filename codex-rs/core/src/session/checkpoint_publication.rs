//! Canonical checkpoint publication, separate from model-context selection.

use super::*;
use codex_extension_api::RestoredSkillsInventory;

#[cfg(test)]
#[path = "user_agent_checkpoint_tests.rs"]
mod tests;

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
        let _observation = self
            .services
            .agent_control
            .acquire_response_observation_transaction(self.presentation_id())
            .await;
        let permit = thread_settings::acquire_persistence_lock(self).await;
        self.check_history_publication()?;
        let state = self.state.lock().await;
        let source = state.history.annotated_items();
        // The current canonical source wins over stale compaction requests. A replacement may
        // neither multiply a route singleton nor introduce another source's harness guidance.
        let mut route_sources = HashSet::new();
        let mut reply_routes = source
            .iter()
            .rev()
            .filter(|item| {
                codex_history::persistent_agent_reply_route_source(item)
                    .is_some_and(|source| route_sources.insert(source))
            })
            .cloned()
            .collect::<Vec<_>>();
        reply_routes.reverse();
        items.retain(|item| codex_history::persistent_agent_reply_route_source(item).is_none());
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
        retained.extend(reply_routes);
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
        // Completion registration shares this publication permit with its canonical receipt.
        // Only exact acknowledged payloads may be withheld from live queue-only history.
        for completion in &state.acknowledged_completion_contexts {
            if !completion.pending
                && metadata
                    .completion_source_items
                    .contains(&completion.item.item)
            {
                // The successful request saw this exact acknowledged payload. Its replacement
                // may summarize it; a stale request without the payload cannot make this cut.
                continue;
            }
            items.retain(|item| item != &completion.item);
            if !completion.pending {
                retained.push(completion.item.clone());
            }
        }
        let task_contexts = self
            .response_observation_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_contexts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let trusted_tasks = task_contexts
            .iter()
            .filter_map(|task| task.item.id().map(|id| (id.clone(), task.item.clone())))
            .collect();
        // Model replacements and extension contributions are not canonical task authority.
        // Preserve reserved identity only for the exact acknowledged envelope.
        completion_replay::normalize_unproven_tasks(&mut items, &trusted_tasks);
        completion_replay::normalize_unproven_tasks(&mut retained, &trusted_tasks);
        for task in &task_contexts {
            if source.contains(&task.item)
                && !metadata.completion_source_items.contains(&task.item.item)
            {
                items.retain(|item| item != &task.item);
                retained.push(task.item.clone());
            }
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
        let mut canonical_items = items.clone();
        canonical_items.extend(
            state
                .acknowledged_completion_contexts
                .iter()
                .filter(|completion| completion.pending)
                .map(|completion| completion.item.clone()),
        );
        let canonical_has_pending = canonical_items.len() != items.len();
        let observations = self
            .services
            .agent_control
            .response_observation_snapshots_for_parent(self.presentation_id());
        let mut observation_artifacts = Vec::new();
        for task in task_contexts {
            if canonical_items.contains(&task.item) {
                observation_artifacts.push(RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                });
                observation_artifacts.push(RolloutItem::ResponseItem(task.item));
                observation_artifacts.push(RolloutItem::AgentResponseObservation(task.observation));
            }
        }
        for completion in &state.acknowledged_completion_contexts {
            let Some(id) = completion.item.id() else {
                continue;
            };
            if !canonical_items.contains(&completion.item) {
                continue;
            }
            let committed = observations
                .iter()
                .filter(|observation| {
                    observation
                        .committed_delivery_response_item_ids
                        .contains(id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if !committed.is_empty() {
                observation_artifacts.push(RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                });
                observation_artifacts.push(RolloutItem::ResponseItem(completion.item.clone()));
                observation_artifacts.extend(
                    committed
                        .into_iter()
                        .map(RolloutItem::AgentResponseObservation),
                );
            }
        }
        observation_artifacts.extend(
            observations
                .into_iter()
                .map(RolloutItem::AgentResponseObservation),
        );
        let mut projected = state.history.clone();
        projected.replace_compacted(
            canonical_items.clone(),
            metadata.reviewer_compaction_hash.as_deref(),
        );
        let compacted_item = CompactedItem {
            message: metadata.message,
            replacement_history: Some(canonical_items.clone()),
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
            latest_token_usage_record: if canonical_has_pending {
                None
            } else {
                state.latest_token_usage_record.clone()
            },
            replacement_history_media_sanitized_prefix_len: Some(
                u64::try_from(canonical_items.len()).unwrap_or(u64::MAX),
            ),
            replacement_history_media_repair: false,
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
        rollout_items.extend(observation_artifacts);
        let receiver = self.dispatch_history_publication(
            permit,
            rollout_items,
            contributions,
            /*acknowledgement*/ None,
            move |state| {
                state.acknowledged_completion_contexts.retain(|completion| {
                    completion.pending
                        || !metadata
                            .completion_source_items
                            .contains(&completion.item.item)
                });
                let installed = items.clone();
                let compacted_prefix_len = items.len();
                state.replace_annotated_history(
                    items,
                    reference_context_item,
                    HistoryReplacement::Compaction {
                        reviewer_compaction_hash: metadata.reviewer_compaction_hash,
                    },
                );
                state.reasoning_effort_pin = ReasoningEffortPin::Compacted;
                state
                    .history
                    .set_compacted_prefix_len(Some(compacted_prefix_len));
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
