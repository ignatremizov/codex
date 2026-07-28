//! Canonical checkpoint publication, separate from model-context selection.

use super::*;
use codex_extension_api::RestoredSkillsInventory;

impl Session {
    pub(super) async fn publish_compacted_history<'a>(
        &self,
        mut items: Vec<ResponseItemEnvelope>,
        reference_context_item: TurnContextItem,
        mut world_state_baseline: WorldStateSnapshot,
        rejected_audio_file_ids: std::collections::BTreeSet<String>,
        installation: CompactedHistoryInstallation<'a>,
    ) -> CodexResult<Option<Vec<ResponseItemEnvelope>>> {
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
        let (
            compacted_item,
            metadata,
            started_item,
            advance_window,
            mcp_revision,
            compacted_prefix_len,
        ) = {
            let mut state = self.state.lock().await;
            let mut unsummarized_suffix = Vec::new();
            let advance_window =
                matches!(&installation, CompactedHistoryInstallation::Prefix { .. });
            let (metadata, started_item) = match installation {
                CompactedHistoryInstallation::Complete(metadata) => (metadata, None),
                CompactedHistoryInstallation::Prefix {
                    prefix,
                    prefix_context,
                    initial_context,
                    reset_world_state_sections,
                    model_info,
                    completion_source_items,
                    compaction_response_id,
                    compaction_summary_tokens,
                    reviewer_compaction_hash,
                    turn_context,
                    started_item,
                } => {
                    if state.history.history_version() != prefix.generation
                        || state.history.annotated_items().len() < prefix.len
                        || self.services.model_client.auth_owner() != prefix.auth_owner
                    {
                        return Ok(None);
                    }
                    let suffix = &state.history.annotated_items()[prefix.len..];
                    if suffix.iter().any(|envelope| {
                        crate::compact_retained_tool_outputs::is_retained_tool_output(envelope)
                            || compact::is_real_user_or_agent_message(&envelope.item)
                    }) {
                        items.extend(initial_context);
                    } else {
                        items = compact::insert_initial_context_before_last_real_user_or_summary(
                            items,
                            initial_context,
                        );
                    }
                    items.splice(
                        0..0,
                        prefix_context.into_iter().map(ResponseItemEnvelope::new),
                    );
                    unsummarized_suffix.extend_from_slice(suffix);
                    if let Some(checkpoint) = state.history.world_state_checkpoint() {
                        world_state_baseline = WorldStateSnapshot::from(&checkpoint.state);
                    }
                    // The suffix may contain old host state, but this new window must
                    // refresh every volatile section selected by the compaction caller.
                    for section_id in reset_world_state_sections {
                        world_state_baseline.remove_section(section_id);
                    }
                    let (window_number, window_ids) = state.prepare_auto_compact_window();
                    (
                        CompactedHistoryMetadata {
                            input_goal_ids: Default::default(),
                            completion_source_items,
                            message: String::new(),
                            compaction_summary_tokens,
                            window_number,
                            window_ids,
                            compaction_response_id: Some(compaction_response_id),
                            compaction_model_hash: model_info.comp_hash.clone(),
                            reviewer_compaction_hash,
                        },
                        started_item.map(|item| (turn_context, item)),
                    )
                }
            };
            // A complete replacement must retain authorization accepted after its input.
            // Prefix replacement already contains every edit from the unchanged suffix.
            if !advance_window {
                let replacement_goal_ids = crate::context::UserGoalUpdate::message_ids(
                    items.iter().map(|envelope| &envelope.item),
                );
                items.extend(
                    state
                        .history
                        .annotated_items()
                        .iter()
                        .filter(|envelope| {
                            crate::context::UserGoalUpdate::message_text(&envelope.item).is_some()
                                && envelope.item.id().is_some_and(|id| {
                                    !metadata.input_goal_ids.contains(id)
                                        && !replacement_goal_ids.contains(id)
                                })
                        })
                        .cloned(),
                );
            }
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
                if !crate::context::McpServerUseInstructions::matches_response_item(&envelope.item)
                {
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
            // The same accepted inventories are already retained above. Keep every other
            // suffix envelope outside the compaction representation gate.
            unsummarized_suffix.retain(|envelope| {
                !crate::context::McpServerUseInstructions::matches_response_item(&envelope.item)
                    && RestoredSkillsInventory::from_envelope(envelope).is_none()
            });
            // Only newly constructed replacement items need identity. Accepted source
            // envelopes must survive byte-for-byte, including legacy absent IDs.
            for envelope in &mut items {
                self.assign_missing_response_item_id(&mut envelope.item);
            }
            if let Some(inventory) = inventory {
                retained.push(inventory.envelope().clone());
            }
            let mut additional_context = crate::state::AdditionalContextStore::default();
            for contribution in &mut contributions {
                let context = contribution.take_additional_context();
                for entry in context.values() {
                    entry
                        .validate()
                        .map_err(|message| CodexErr::InvalidRequest(message.into()))?;
                }
                let format = if self.enabled(Feature::UntrustedAsToolOutput) {
                    crate::state::AdditionalContextFormat::ToolOutputs
                } else {
                    crate::state::AdditionalContextFormat::Messages
                };
                // Preserve trust/metadata in the ordinary additional-context renderer; this
                // disposable checkpoint rendering does not mutate the live client context map.
                retained.extend(
                    additional_context
                        .extend(context, format, &self.services.session_telemetry)
                        .into_iter()
                        .map(|mut envelope| {
                            self.assign_missing_response_item_id(&mut envelope.item);
                            envelope
                        }),
                );
                retained.extend(contribution.take_items().into_iter().map(|mut item| {
                    self.assign_missing_response_item_id(&mut item);
                    ResponseItemEnvelope::new(item)
                }));
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
                if !completion.pending && !unsummarized_suffix.contains(&completion.item) {
                    retained.push(completion.item.clone());
                }
            }
            let retained_from_source = retained
                .iter()
                .map(|envelope| {
                    source.contains(envelope)
                        || state
                            .acknowledged_completion_contexts
                            .iter()
                            .any(|completion| &completion.item == envelope)
                })
                .collect::<Vec<_>>();
            let boundary = items
                .iter()
                .position(|envelope| {
                    matches!(
                        &envelope.item,
                        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                    ) || matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user")
                })
                .unwrap_or(items.len());
            // Attribute a replacement-owned item, not an accepted inventory envelope.
            // Retained MCP/skills records keep their original metadata and identities.
            let mcp_revision = self
                .services
                .executed_tool_calls
                .mcp_attribution_checkpoint(/*force*/ true)
                .and_then(|(attribution, revision)| {
                    items.last_mut().map(|envelope| {
                        envelope.metadata.get_or_insert_default().mcp_attribution =
                            Some(attribution);
                        revision
                    })
                });
            // Checkpoint current policy and sampling on replacement-owned records. Original
            // inventories retain their complete metadata, including older sampling watermarks.
            crate::model_search::SearchOwner::checkpoint(
                &self.services.thread_extension_data,
                &mut items,
            );
            state.history.checkpoint_sampling(&mut items);
            for (envelope, from_source) in retained.iter_mut().zip(retained_from_source) {
                if !from_source {
                    state
                        .history
                        .checkpoint_sampling(std::slice::from_mut(envelope));
                }
            }
            items.splice(boundary..boundary, retained);
            // Every caller, including a fresh context-window reset, passes the same
            // representation gate before this checkpoint certifies its compacted prefix.
            // A prepared prefix can have newer, still-unsummarized media after this boundary.
            let prefix_len = items.len();
            crate::context::sanitize_compacted_media_envelopes(&mut items, prefix_len);
            items.extend(unsummarized_suffix.iter().cloned());
            if let Some(checkpoint) = items.iter_mut().rev().find(|envelope| {
                matches!(
                    envelope.item,
                    ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                )
            }) {
                checkpoint
                    .metadata
                    .get_or_insert_default()
                    .compaction_model_hash = metadata.compaction_model_hash.clone();
            }
            let mut canonical_items = items.clone();
            canonical_items.splice(
                prefix_len..prefix_len,
                state
                    .acknowledged_completion_contexts
                    .iter()
                    .filter(|completion| completion.pending)
                    .map(|completion| completion.item.clone()),
            );
            let canonical_has_pending = canonical_items.len() != items.len();
            let canonical_prefix_len = prefix_len + canonical_items.len() - items.len();
            crate::context::sanitize_compacted_media_envelopes(
                &mut canonical_items,
                canonical_prefix_len,
            );
            let mut projected = state.history.clone();
            projected.replace_compacted(
                canonical_items.clone(),
                metadata.reviewer_compaction_hash.as_deref(),
            );
            projected.extend_rejected_audio_file_ids(rejected_audio_file_ids.clone());
            let compacted_item = CompactedItem {
                message: metadata.message.clone(),
                replacement_history: Some(canonical_items.clone()),
                retained_context: Some(projected.retained_context().clone()),
                guardian_history: projected
                    .compacted_guardian_history_checkpoint(&unsummarized_suffix),
                mcp_resource_origins: self.services.mcp_runtime.resource_origin_checkpoint(),
                compaction_summary_tokens: metadata.compaction_summary_tokens,
                window_number: Some(metadata.window_number),
                first_window_id: Some(metadata.window_ids.first_window_id.clone()),
                previous_window_id: metadata.window_ids.previous_window_id.clone(),
                window_id: Some(metadata.window_ids.window_id.clone()),
                compaction_response_id: metadata.compaction_response_id.clone(),
                latest_token_usage_record: if canonical_has_pending {
                    None
                } else {
                    state.latest_token_usage_record.clone().map(Box::new)
                },
                resume_metadata: Some(CompactionResumeMetadata {
                    rejected_audio_file_ids: projected.rejected_audio_file_ids(),
                    dynamic_tools: Some(Box::new(DynamicToolsSnapshot {
                        thread_id: self.thread_id(),
                        tools: state.persisted_dynamic_tools.to_vec(),
                    })),
                    multi_agent_version: self.multi_agent_version(),
                    last_started_turn_id: state.last_started_turn_id.clone(),
                    turn_attribution: state.turn_attribution.clone(),
                    previous_turn_settings: state.previous_turn_settings(),
                }),
                replacement_history_media_sanitized_prefix_len: Some(
                    u64::try_from(canonical_prefix_len).unwrap_or(u64::MAX),
                ),
                replacement_history_media_repair: false,
            };
            (
                compacted_item,
                metadata,
                started_item,
                advance_window,
                mcp_revision,
                prefix_len,
            )
        };
        if let Some((turn_context, item)) = started_item {
            // Reject stale candidates before announcing installation or touching storage.
            self.emit_turn_item_started(turn_context, item).await;
        }
        let mut rollout_items = vec![RolloutItem::Compacted(Box::new(compacted_item))];
        rollout_items.push(RolloutItem::WorldState(WorldStateItem::full(
            world_state_baseline.clone().into_object(),
        )));
        rollout_items.push(RolloutItem::TurnContext(reference_context_item.clone()));
        rollout_items.push(RolloutItem::EventMsg(
            thread_settings::applied_event(self).await,
        ));
        let executed_tool_calls = self.services.executed_tool_calls.clone();
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
                if advance_window {
                    state.install_prepared_auto_compact_window(
                        metadata.window_number,
                        metadata.window_ids,
                    );
                }
                state
                    .history
                    .extend_rejected_audio_file_ids(rejected_audio_file_ids);
                state.replace_annotated_history(
                    items,
                    Some(reference_context_item),
                    HistoryReplacement::Compaction {
                        reviewer_compaction_hash: metadata.reviewer_compaction_hash,
                    },
                );
                if let Some(sequence) = state
                    .acknowledged_completion_contexts
                    .iter()
                    .filter_map(|completion| completion.item.metadata.as_ref()?.history_sequence)
                    .max()
                {
                    state.history.reserve_input_sequence_through(sequence);
                }
                state.reasoning_effort_pin = ReasoningEffortPin::Compacted;
                state.history.set_world_state_baseline(world_state_baseline);
                state
                    .history
                    .set_compacted_prefix_len(Some(compacted_prefix_len));
                state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
                if let Some(revision) = mcp_revision {
                    executed_tool_calls.mark_mcp_attribution_persisted(revision);
                }
                installed
            },
        )?;
        self.publication_result(receiver).await.map(Some)
    }
}
