use super::TurnInput as PendingTurnInput;
use super::session::Session;
use super::transcript_publication::ConversationBoundary;
use super::turn_context::TurnContext;
use codex_analytics::ImagePreparationMetadata;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;

impl Session {
    /// Returns the input if there is no active turn to inject into.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn inject_if_running<T: Into<ResponseItemEnvelope>>(
        &self,
        input: Vec<T>,
    ) -> Result<(), Vec<T>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(active_turn) => {
                self.input_queue
                    .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                        active_turn.turn_state.as_ref(),
                        input
                            .into_iter()
                            .map(Into::into)
                            .map(PendingTurnInput::ResponseItem)
                            .collect(),
                    )
                    .await;
                Ok(())
            }
            None => Err(input),
        }
    }

    /// Injects hook context into the running turn atomically.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn provenance and turn state updates must remain atomic"
    )]
    pub(crate) async fn inject_hook_context_if_running(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), Vec<ResponseItem>> {
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(input);
        };
        if active_turn.task.is_none() {
            return Err(input);
        }
        self.input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                active_turn.turn_state.as_ref(),
                input
                    .into_iter()
                    .map(ResponseItemEnvelope::new)
                    .map(PendingTurnInput::ResponseItem)
                    .collect(),
            )
            .await;
        Ok(())
    }

    /// Assigns host identity to new public agent input and preserves client provenance.
    /// Supplied agent names remain display provenance, not authorization.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn inject_client_response_items(
        &self,
        items: Vec<ResponseItem>,
        turn_context: &TurnContext,
    ) -> CodexResult<()> {
        let _admission = self.submission_admission.admit_injection().await?;
        self.check_history_publication()?;
        let items = items
            .into_iter()
            .map(|mut item| {
                if let ResponseItem::AgentMessage {
                    id,
                    internal_chat_message_metadata_passthrough,
                    ..
                } = &mut item
                {
                    *id = Some(ResponseItemId::new("amsg"));
                    *internal_chat_message_metadata_passthrough = None;
                }
                self.annotate_client_response_item(item)
            })
            .collect::<Vec<_>>();
        let mut active = self.active_turn.lock().await;
        if let Some(active_turn) = active.as_mut() {
            // Queue acceptance owns these fresh public items. Attribution is installed
            // by the actual consuming turn, never trusted from caller metadata.
            self.input_queue
                .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                    active_turn.turn_state.as_ref(),
                    items
                        .into_iter()
                        .map(PendingTurnInput::ResponseItem)
                        .collect(),
                )
                .await;
            return Ok(());
        }
        drop(active);
        let boundary = if items
            .iter()
            .any(|item| matches!(&item.item, ResponseItem::AgentMessage { .. }))
        {
            ConversationBoundary::HistoryOnly
        } else {
            ConversationBoundary::Existing
        };
        let (items, preparations) = self
            .prepare_annotated_conversation_items_for_history(
                turn_context,
                turn_context.model_info(),
                items,
            )
            .await;
        self.record_prepared_conversation_items(
            turn_context,
            turn_context.model_info(),
            items,
            preparations,
            /*acknowledgement*/ None,
            boundary,
        )
        .await
    }

    pub(crate) fn annotate_client_response_item(&self, item: ResponseItem) -> ResponseItemEnvelope {
        let metadata = (self.enabled(Feature::RetainClientDeveloperMessages)
            && matches!(&item, ResponseItem::Message { role, .. } if role == "developer"))
        .then_some(CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        });

        ResponseItemEnvelope { item, metadata }
    }

    pub(crate) async fn record_annotated_conversation_items(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        items: Vec<ResponseItemEnvelope>,
    ) {
        if items.iter().all(|item| item.metadata.is_none()) {
            let items = items
                .into_iter()
                .map(ResponseItemEnvelope::into_item)
                .collect::<Vec<_>>();
            self.record_conversation_items(turn_context, model_info, &items)
                .await;
            return;
        }

        let (annotated_items, image_preparations) = self
            .prepare_annotated_conversation_items_for_history(turn_context, model_info, items)
            .await;
        if let Err(error) = self
            .record_prepared_conversation_items(
                turn_context,
                model_info,
                annotated_items,
                image_preparations,
                /*acknowledgement*/ None,
                ConversationBoundary::Existing,
            )
            .await
        {
            tracing::error!("failed to publish annotated conversation items: {error}");
        }
    }

    pub(super) async fn prepare_annotated_conversation_items_for_history(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        items: Vec<ResponseItemEnvelope>,
    ) -> (Vec<ResponseItemEnvelope>, Vec<ImagePreparationMetadata>) {
        let mut annotated_items = Vec::with_capacity(items.len());
        let mut image_preparations = Vec::new();
        for envelope in items {
            let (prepared_items, prepared_images) = self
                .prepare_conversation_items_for_history(
                    turn_context,
                    model_info,
                    std::slice::from_ref(&envelope.item),
                )
                .await;
            image_preparations.extend(prepared_images);

            let mut metadata = envelope.metadata;
            annotated_items.extend(prepared_items.into_owned().into_iter().map(|item| {
                ResponseItemEnvelope {
                    item,
                    metadata: metadata.take(),
                }
            }));
        }
        (annotated_items, image_preparations)
    }

    /// Injects items into active work, or records them without starting a turn.
    pub(crate) async fn inject_no_new_turn(
        &self,
        items: Vec<ResponseItem>,
        current_turn_context: Option<&TurnContext>,
    ) {
        let Err(items) = self.inject_if_running(items).await else {
            return;
        };
        let default_turn_context;
        let turn_context = match current_turn_context {
            Some(turn_context) => turn_context,
            None => {
                default_turn_context = self.new_default_turn().await;
                default_turn_context.as_ref()
            }
        };
        let boundary = if items
            .iter()
            .any(|item| matches!(item, ResponseItem::AgentMessage { .. }))
        {
            ConversationBoundary::HistoryOnly
        } else {
            ConversationBoundary::Existing
        };
        let (items, preparations) = self
            .prepare_conversation_items_for_history(turn_context, turn_context.model_info(), &items)
            .await;
        if let Err(error) = self
            .record_prepared_conversation_items(
                turn_context,
                turn_context.model_info(),
                items
                    .into_owned()
                    .into_iter()
                    .map(ResponseItemEnvelope::new)
                    .collect(),
                preparations,
                /*acknowledgement*/ None,
                boundary,
            )
            .await
        {
            tracing::error!("failed to publish injected conversation items: {error}");
        }
    }
}

#[cfg(test)]
#[path = "inject_tests.rs"]
mod tests;
