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
use codex_protocol::models::MessagePhase;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::AgentInputPresentation;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_protocol::user_input::UserInput;
use codex_thread_store::PersistContext;
use std::collections::HashMap;

impl Session {
    pub(crate) async fn record_prompt_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        input: &[UserInput],
        client_id: Option<String>,
        acceptance_order: Option<u64>,
        persist_context: PersistContext,
        prompt_kind: PromptInputKind,
        additional_context: Vec<ResponseItemEnvelope>,
    ) -> CodexResult<()> {
        let mut image_positions = HashMap::new();
        let response_item = self.response_item_from_user_input_with_image_positions(
            input.to_vec(),
            &mut image_positions,
        );
        let mut items = vec![ResponseItemEnvelope {
            item: response_item,
            metadata: acceptance_order.map(|order| CodexHarnessMetadata {
                user_input_order: Some(order),
                ..Default::default()
            }),
        }];
        items.extend(additional_context);
        let (prepared, images) = self
            .prepare_annotated_conversation_items_for_history(turn_context, model_info, items)
            .await;
        let turn_item = match prompt_kind {
            PromptInputKind::User => {
                let mut item = UserMessageItem::new(input);
                apply_prepared_image_file_ids(&mut item, &prepared, &image_positions);
                item.client_id = client_id;
                Some(TurnItem::UserMessage(item))
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::AttributedInput { attribution, input },
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
            } => {
                let mut item = AgentMessageItem::new(&[AgentMessageContent::Text { text }]);
                item.id = new_attributed_agent_message_response_item_id().to_string();
                item.phase = Some(MessagePhase::Commentary);
                Some(TurnItem::AgentMessage(item))
            }
            PromptInputKind::Agent {
                presentation: AgentInputPresentation::Delegated(visible),
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
                }).cloned(),
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
