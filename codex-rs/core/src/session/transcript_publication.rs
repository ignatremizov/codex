//! Canonical conversation publication and history-only transcript boundaries.

use super::Session;
use super::durable_context::PublicationBatch;
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
use codex_utils_output_truncation::with_serialization_allowance;

pub(super) enum ConversationBoundary {
    Existing,
    HistoryOnly,
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
        let response_items = items
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect::<Vec<_>>();
        {
            let mut state = self.state.lock().await;
            if self.guardian_context_mode == crate::context::GuardianContextMode::ThreadOwned {
                for envelope in &mut items {
                    if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "assistant")
                        || matches!(&envelope.item, ResponseItem::FunctionCall { .. })
                        || crate::context::is_user_authorization_message(&envelope.item)
                    {
                        // Share accepted input order with recorded assistant messages and calls.
                        // A call's result can arrive after a reply; it must not move the question.
                        envelope
                            .metadata
                            .get_or_insert_default()
                            .user_input_order
                            .get_or_insert_with(|| state.history.reserve_input_order());
                    }
                }
            }
        }
        let mut rollout_items: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
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
        let truncation_policy = model_info.truncation_policy.into();
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
        let batch = self
            .conversation_publication_batch(turn_context, rollout_items, &response_items)
            .await;
        let analytics = self.services.analytics_events_client.clone();
        let turn_id = turn_context.sub_id.clone();
        let receiver = self.dispatch_history_publication_with_events(
            permit,
            batch,
            Vec::new(),
            acknowledgement,
            move |state| {
                state.current_time_reminder.note_recorded_items(&recorded);
                state
                    .history
                    .record_annotated_items(&items, truncation_policy);
                for image in image_preparations {
                    analytics.track_image_preparation(ImagePreparationFact {
                        turn_id: turn_id.clone(),
                        metadata: image,
                    });
                }
            },
        )?;
        self.publication_result(receiver).await
    }
}
