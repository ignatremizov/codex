//! Canonical publication of prepared agent input with separately retained audit.
//!
//! Exact turn admission happens in `turn_input`; this owner publishes the model
//! payload and attributed presentation together. It never fabricates human input,
//! exposes ciphertext as readable text, or treats legacy mailbox acceptance as
//! proof of either admission or consumption.

use super::Session;
use super::UserInputMetadata;
use super::transcript_publication::ConversationBoundary;
use super::turn_context::TurnContext;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::PreparedAgentInput;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItemMetadata;
use codex_protocol::models::ContentItemNamespace;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_thread_store::PersistContext;

impl Session {
    pub(super) fn validate_prepared_agent_input(
        &self,
        input: &PreparedAgentInput,
    ) -> CodexResult<()> {
        input
            .validate_receiver(self.thread_id())
            .map_err(|message| CodexErr::InvalidRequest(message.to_owned()))?;
        if input.recipient_instance_id != self.presentation_id().instance_id {
            return Err(CodexErr::InvalidRequest(
                "prepared agent input belongs to a previous receiver incarnation".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn record_prepared_agent_input(
        &self,
        turn: &TurnContext,
        model_info: &ModelInfo,
        input: Box<PreparedAgentInput>,
        metadata: UserInputMetadata,
        persist_context: PersistContext,
    ) -> CodexResult<()> {
        self.validate_prepared_agent_input(&input)?;
        let mut model_input = input.communication.to_model_input_item();
        if let ResponseItem::AgentMessage {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut model_input
        {
            internal_chat_message_metadata_passthrough
                .get_or_insert_default()
                .content_item_metadata = Some(vec![
                ContentItemMetadata::tool(Some(
                    turn.config
                        .multi_agent_v2
                        .tool_namespace
                        .clone()
                        .map(ContentItemNamespace::from)
                        .unwrap_or(ContentItemNamespace::Functions),
                ));
                content.len()
            ]);
        }
        let readable = input
            .readable_content()
            .map(|text| AgentMessageContent::Text {
                text: text.to_owned(),
            })
            .into_iter()
            .collect::<Vec<_>>();
        let mut presentation = AgentMessageItem::new(&readable);
        presentation.id = new_attributed_agent_message_response_item_id().to_string();
        presentation.attribution = Some(input.attribution.as_ref().clone());
        presentation.phase = Some(MessagePhase::Commentary);
        // No synthetic UserInput is used for either opaque bytes or readable audit.
        let envelope = ResponseItemEnvelope {
            item: model_input,
            metadata: Some(CodexHarnessMetadata {
                user_input_order: metadata.acceptance_order,
                prepared_agent_input: Some(input),
                ..Default::default()
            }),
        };
        let (prepared, images) = self
            .prepare_annotated_conversation_items_for_history(turn, model_info, vec![envelope])
            .await;
        self.record_prepared_conversation_items(
            turn,
            model_info,
            prepared,
            images,
            /*acknowledgement*/ None,
            ConversationBoundary::Prompt {
                presentation: Some(Box::new(TurnItem::AgentMessage(presentation))),
            },
        )
        .await?;
        self.ensure_rollout_materialized(persist_context).await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "prepared_agent_input_tests.rs"]
mod tests;
