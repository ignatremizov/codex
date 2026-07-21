use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;

use super::InterAgentMessageSource;
use super::ThreadItem;

const OPAQUE_MESSAGE: &str = "Input message encrypted";
const MESSAGE_TYPE_PREFIX: &str = "Message Type: ";
const TASK_NAME_SEPARATOR: &str = "\nTask name: ";
const SENDER_SEPARATOR: &str = "\nSender: ";
const PAYLOAD_SEPARATOR: &str = "\nPayload:\n";

/// Converts a model-visible inter-agent response item into its transcript representation.
pub fn inter_agent_message_thread_item(item: &ResponseItem) -> Option<ThreadItem> {
    let id = item.id()?.to_string();
    inter_agent_message_thread_item_with_id(item, id)
}

pub(crate) fn inter_agent_message_thread_item_with_id(
    item: &ResponseItem,
    id: String,
) -> Option<ThreadItem> {
    let ResponseItem::AgentMessage {
        author,
        recipient,
        content,
        ..
    } = item
    else {
        return None;
    };
    if id.is_empty() {
        return None;
    }
    let text = if content
        .iter()
        .any(|part| matches!(part, AgentMessageInputContent::EncryptedContent { .. }))
    {
        OPAQUE_MESSAGE.to_string()
    } else {
        codex_protocol::models::plaintext_agent_message_content(content)?
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(ThreadItem::AgentMessage {
        id,
        text: transcript_text(author, recipient, &text),
        inter_agent_source: Some(InterAgentMessageSource {
            author: author.clone(),
            recipient: recipient.clone(),
        }),
        phase: Some(codex_protocol::models::MessagePhase::Commentary),
        memory_citation: None,
        delivery: None,
        questions: None,
    })
}

fn transcript_text(author: &str, recipient: &str, text: &str) -> String {
    match envelope_payload(author, recipient, text) {
        Some(("FINAL_ANSWER", payload)) => {
            format!("Agent final answer from `{author}`:\n\n{payload}")
        }
        Some(("MESSAGE" | "NEW_TASK", payload)) => {
            format!("Agent message from `{author}`:\n\n{payload}")
        }
        Some(_) | None => format!("Agent message from `{author}`:\n\n{text}"),
    }
}

fn envelope_payload<'a>(
    author: &str,
    recipient: &str,
    text: &'a str,
) -> Option<(&'a str, &'a str)> {
    let envelope = text.strip_prefix(MESSAGE_TYPE_PREFIX)?;
    let (message_type, envelope) = envelope.split_once(TASK_NAME_SEPARATOR)?;
    let (task_name, envelope) = envelope.split_once(SENDER_SEPARATOR)?;
    let (sender, payload) = envelope.split_once(PAYLOAD_SEPARATOR)?;
    if !matches!(message_type, "MESSAGE" | "NEW_TASK" | "FINAL_ANSWER") {
        return None;
    }
    (task_name == recipient && sender == author).then_some((message_type, payload))
}

#[cfg(test)]
#[path = "inter_agent_message_tests.rs"]
mod tests;
