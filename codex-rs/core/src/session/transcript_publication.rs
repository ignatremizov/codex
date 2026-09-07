//! Canonical conversation publication and history-only transcript boundaries.

use super::Session;
use super::durable_context::PublicationBatch;
use super::prepared_history_items;
use super::retained_context;
use super::thread_settings;
use super::turn_context::TurnContext;
use crate::stream_events_utils::mark_thread_memory_mode_polluted_if_external_context;
use codex_analytics::ImagePreparationFact;
use codex_analytics::ImagePreparationMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::should_persist_response_item;
use codex_utils_output_truncation::with_serialization_allowance;

pub(super) enum ConversationBoundary {
    Existing,
    HistoryOnly,
    Prompt {
        presentation: Option<Box<codex_protocol::items::TurnItem>>,
    },
}

impl Session {
    pub(super) async fn conversation_publication_batch(
        &self,
        turn_context: &TurnContext,
        rollout: Vec<RolloutItem>,
        items: &[ResponseItem],
    ) -> PublicationBatch {
        let raw_event_guardian_thread_id =
            if matches!(
                &turn_context.session_source,
                SessionSource::SubAgent(SubAgentSource::Other(name))
                    if name == crate::guardian::GUARDIAN_REVIEWER_NAME
            ) && self.services.analytics_events_client.is_enabled()
                && turn_context.parent_thread_id.is_some()
                && self.is_private_guardian_reviewer().await
            {
                Some(self.thread_id)
            } else {
                None
            };
        PublicationBatch {
            rollout,
            reply: None,
            events: items
                .iter()
                .map(|item| Event {
                    id: turn_context.sub_id.clone(),
                    msg: EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
                })
                .collect(),
            raw_event_guardian_thread_id,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(super) async fn record_prepared_conversation_items(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        mut items: Vec<ResponseItemEnvelope>,
        image_preparations: Vec<ImagePreparationMetadata>,
        acknowledgement: Option<codex_extension_api::TurnInputContributionAcknowledgement>,
        boundary: ConversationBoundary,
    ) -> CodexResult<()> {
        let permit = thread_settings::acquire_persistence_lock(self).await;
        self.check_history_publication()?;
        // Save the originating history budget for replay.
        // Preserve any existing tool-specific override.
        let policy: codex_utils_output_truncation::TruncationPolicy =
            model_info.truncation_policy.into();
        for envelope in &mut items {
            if matches!(
                envelope.item,
                ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
            ) {
                envelope
                    .metadata
                    .get_or_insert_default()
                    .history_truncation_token_limit
                    .get_or_insert_with(|| with_serialization_allowance(policy).token_budget());
            }
        }
        // Last-N-turn forks retain a suffix starting at a user turn boundary. Repeat the
        // cumulative checkpoint there so the suffix remains self-contained even when the fork
        // happens mid-turn; dirty checkpoints preserve new sources between boundaries.
        let force_mcp_checkpoint = items
            .iter()
            .any(|envelope| crate::context_manager::is_user_turn_boundary(&envelope.item));
        let mcp_revision = self
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(force_mcp_checkpoint)
            .and_then(|(attribution, revision)| {
                let first_persisted = items
                    .iter()
                    .position(|envelope| should_persist_response_item(&envelope.item))?;
                for (index, envelope) in items.iter_mut().enumerate() {
                    // Rollout batches may be partially written. Checkpoint the first persisted
                    // item, and repeat the checkpoint at turn boundaries retained by forks.
                    if index == first_persisted
                        || crate::context_manager::is_user_turn_boundary(&envelope.item)
                    {
                        envelope.metadata.get_or_insert_default().mcp_attribution =
                            Some(attribution.clone());
                    }
                }
                Some(revision)
            });
        let response_items = items
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect::<Vec<_>>();
        let prepared = {
            let mut state = self.state.lock().await;
            let pending_orders = turn_context
                .extension_data
                .get::<retained_context::PendingAssistantMessageOrders>();
            for envelope in &mut items {
                if envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.compaction_output)
                {
                    continue;
                }
                if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "assistant")
                    || matches!(&envelope.item, ResponseItem::FunctionCall { .. })
                    || crate::context::is_user_authorization_message(&envelope.item)
                {
                    let message_order = pending_orders.as_ref().and_then(|orders| {
                        orders
                            .0
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(envelope.item.id()?.as_str())
                    });
                    // Preserve input acceptance and source-message start order.
                    // Synthetic messages still receive their order here.
                    envelope
                        .metadata
                        .get_or_insert_default()
                        .user_input_order
                        .get_or_insert_with(|| {
                            message_order.unwrap_or_else(|| state.history.reserve_input_order())
                        });
                }
            }
            prepared_history_items::PreparedHistoryItems::new(&state.history, items, policy)
        };
        let mut rollout_items = prepared.rollout_items();
        // Mark external context before transferring work to the independent worker.
        // A cancelled result waiter must not bypass the memory privacy gate.
        if turn_context.config.memories.disable_on_external_context
            && let Some(item) = response_items
                .iter()
                .find(|item| matches!(item, ResponseItem::FunctionCallOutput { call_id: None, .. }))
        {
            mark_thread_memory_mode_polluted_if_external_context(self, turn_context, item).await;
        }
        let recorded = response_items.clone();
        let executed_tool_calls = self.services.executed_tool_calls.clone();
        if matches!(boundary, ConversationBoundary::HistoryOnly) {
            rollout_items.insert(
                0,
                RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: turn_context.sub_id.clone(),
                    root_turn_id: None,
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                    agent_queue: None,
                })),
            );
            rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
                TurnCompleteEvent {
                    turn_id: turn_context.sub_id.clone(),
                    last_agent_message: None,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                },
            )));
        }
        let mut batch = self
            .conversation_publication_batch(turn_context, rollout_items, &response_items)
            .await;
        let analytics = self.services.analytics_events_client.clone();
        let turn_id = turn_context.sub_id.clone();
        let prompt_receipt = if matches!(&boundary, ConversationBoundary::Prompt { .. }) {
            self.take_queued_input_persistence(&turn_id)
        } else {
            None
        };
        let presentation = match boundary {
            ConversationBoundary::Prompt { presentation } => {
                presentation.map(|presentation| *presentation)
            }
            ConversationBoundary::Existing | ConversationBoundary::HistoryOnly => None,
        };
        if let Some(item) = &presentation {
            let completed_at_ms = crate::turn_timing::now_unix_timestamp_ms();
            let event = Event {
                id: turn_id.clone(),
                msg: EventMsg::ItemCompleted(codex_protocol::protocol::ItemCompletedEvent {
                    thread_id: self.thread_id(),
                    turn_id: turn_id.clone(),
                    item: item.clone(),
                    started_at_ms: Some(completed_at_ms),
                    completed_at_ms,
                }),
            };
            batch.rollout.push(RolloutItem::EventMsg(event.msg.clone()));
            batch.events.push(event);
        }
        let audit_control = self.services.local_agent_runtime.control(self.session_id());
        let recipient = self.thread_id();
        let receiver = self.dispatch_history_publication_with_events(
            permit,
            batch,
            Vec::new(),
            acknowledgement,
            move |state| {
                state.current_time_reminder.note_recorded_items(&recorded);
                prepared.install(&mut state.history);
                if let Some(revision) = mcp_revision {
                    executed_tool_calls.mark_mcp_attribution_persisted(revision);
                }
                for image in image_preparations {
                    analytics.track_image_preparation(ImagePreparationFact {
                        turn_id: turn_id.clone(),
                        metadata: image,
                    });
                }
                // The canonical append+flush and live installation precede this receipt.
                // The worker owns it even when the originating task is forcibly cancelled.
                if let Some(receipt) = prompt_receipt {
                    let _ = receipt.send(Ok(()));
                }
                if let Some(item) = presentation {
                    tokio::spawn(async move {
                        if let Err(error) = audit_control
                            .mirror_attributed_agent_input(recipient, &item)
                            .await
                        {
                            tracing::warn!(%error, "failed to present accepted peer input");
                        }
                    });
                }
            },
        )?;
        self.publication_result(receiver).await
    }
}
