//! Remove parent-owned runtime notifications from child model context, not audit history.

use crate::context::AgentReplyRoute;
use crate::context::AttributedAgentMessage;
use crate::context::ContextualUserFragment;
use crate::context::SubagentNotification;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(super) fn retain_without_notification_context(envelope: &mut ResponseItemEnvelope) -> bool {
    if envelope
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.client_authored)
    {
        return true;
    }
    let ResponseItem::Message {
        role,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return true;
    };
    if role != "user"
        || !internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
            .is_some_and(|kinds| {
                kinds.iter().any(|kind| {
                    matches!(
                        kind.0.as_str(),
                        "multi_agent.subagent_notification"
                            | "multi_agent.agent_reply_route"
                            | "multi_agent.persistent_agent_reply_route"
                            | "multi_agent.attributed_agent_message"
                    )
                })
            })
    {
        return true;
    }
    let Some(mut content) = to_annotated_content(&mut envelope.item) else {
        return true;
    };
    content.retain(|fragment| {
        let ContentItem::InputText { text } = fragment.content() else {
            return true;
        };
        match fragment.kind().0.as_str() {
            "multi_agent.subagent_notification" => !SubagentNotification::matches_text(text),
            "multi_agent.agent_reply_route" | "multi_agent.persistent_agent_reply_route" => {
                !AgentReplyRoute::matches_text(text)
            }
            "multi_agent.attributed_agent_message" => !AttributedAgentMessage::matches_text(text),
            _ => true,
        }
    });
    !content.is_empty() && set_annotated_content(&mut envelope.item, content).is_some()
}

#[cfg(test)]
#[path = "fork_notification_context_tests.rs"]
mod tests;
