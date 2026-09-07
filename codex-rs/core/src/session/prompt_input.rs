//! Prompt publication uses the owned canonical writer, independently of task cancellation.

use super::Session;
use super::apply_prepared_image_file_ids;
use super::input_queue::PromptInputKind;
use super::transcript_publication::ConversationBoundary;
use super::turn_context::TurnContext;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::AgentInputPresentation;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_protocol::user_input::UserInput;
use codex_thread_store::PersistContext;
use std::collections::HashMap;

#[cfg(test)]
#[path = "prompt_input_tests.rs"]
mod tests;

impl Session {
    pub(crate) async fn record_prompt_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        input: &[UserInput],
        persist_context: PersistContext,
        prompt_kind: PromptInputKind,
        additional_context: Vec<ResponseItemEnvelope>,
    ) -> CodexResult<()> {
        let mut image_positions = HashMap::new();
        let metadata = match &prompt_kind {
            PromptInputKind::User { metadata, .. } | PromptInputKind::Agent { metadata, .. } => {
                *metadata
            }
        };
        let mut response_item = self.response_item_from_user_input_with_image_positions(
            input.to_vec(),
            &mut image_positions,
        );
        // Preserve the captured input origin, including a heartbeat steered into an existing
        // turn. Synthetic agent presentation must not rewrite the user's input classification.
        if matches!(&prompt_kind, PromptInputKind::User { .. })
            && metadata.origin == codex_history::UserInputOrigin::Heartbeat
            && let ResponseItem::Message {
                content,
                internal_chat_message_metadata_passthrough: Some(passthrough),
                ..
            } = &mut response_item
            && matches!(content.as_slice(), [ContentItem::InputText { .. }])
        {
            passthrough.content_item_kinds = Some(vec![ContentItemKind(
                codex_history::HEARTBEAT_CONTENT_KIND.to_owned(),
            )]);
        }
        let attributed = match &prompt_kind {
            PromptInputKind::Agent { presentation, .. } => {
                !matches!(presentation, AgentInputPresentation::Delegated(_))
            }
            PromptInputKind::User { .. } => false,
        };
        let items = vec![ResponseItemEnvelope {
            item: response_item,
            metadata: metadata.acceptance_order.map(|order| CodexHarnessMetadata {
                user_input_order: Some(order),
                ..Default::default()
            }),
        }];
        let (mut prepared, mut images) = self
            .prepare_annotated_conversation_items_for_history(turn_context, model_info, items)
            .await;
        if attributed {
            // Rich-input preparation can replace media with text. Classify every resulting
            // content item from typed authorship, after preparation but before source capture.
            // Neither an attachment nor its fallback may grant human-input authority.
            for envelope in &mut prepared {
                if let ResponseItem::Message {
                    content,
                    internal_chat_message_metadata_passthrough,
                    ..
                } = &mut envelope.item
                {
                    internal_chat_message_metadata_passthrough
                        .get_or_insert_default()
                        .content_item_kinds = Some(vec![
                        ContentItemKind(
                            "multi_agent.attributed_agent_message".to_string()
                        );
                        content.len()
                    ]);
                }
            }
        }
        // Additional context has its own source classification; do not relabel it as agent mail.
        let (additional, additional_images) = self
            .prepare_annotated_conversation_items_for_history(
                turn_context,
                model_info,
                additional_context,
            )
            .await;
        prepared.extend(additional);
        images.extend(additional_images);
        let turn_item = match prompt_kind {
            PromptInputKind::User { client_id, .. } => {
                let mut item = UserMessageItem::new(input);
                apply_prepared_image_file_ids(&mut item, &prepared, &image_positions);
                item.client_id = client_id;
                Some(TurnItem::UserMessage(item))
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::AttributedInput { attribution, input },
                ..
            } => {
                let mut item = AgentMessageItem::new(&[]);
                item.id = new_attributed_agent_message_response_item_id().to_string();
                item.attribution = Some(*attribution);
                item.input = Some(input);
                item.phase = Some(MessagePhase::Commentary);
                Some(TurnItem::AgentMessage(item))
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::Attributed(text),
                ..
            } => {
                let mut item = AgentMessageItem::new(&[AgentMessageContent::Text { text }]);
                item.id = new_attributed_agent_message_response_item_id().to_string();
                item.phase = Some(MessagePhase::Commentary);
                Some(TurnItem::AgentMessage(item))
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::Delegated(visible),
                ..
            } => {
                (!visible.is_empty()).then(|| TurnItem::UserMessage(UserMessageItem::new(&visible)))
            }
        };
        self.record_prepared_conversation_items(
            turn_context,
            model_info,
            prepared,
            images,
            /*acknowledgement*/ None,
            ConversationBoundary::Prompt {
                presentation: turn_item.as_ref().filter(|item| {
                    matches!(item, TurnItem::AgentMessage(message) if message.attribution.is_some())
                }).cloned().map(Box::new),
            },
        )
        .await?;
        if let Some(item) = turn_item
            && !matches!(&item, TurnItem::AgentMessage(message) if message.attribution.is_some())
        {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
        self.ensure_rollout_materialized(persist_context).await;
        Ok(())
    }
}
