use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use serde::Deserialize;

const MESSAGE_TYPE_PREFIX: &str = "Message Type: ";
const TASK_NAME_SEPARATOR: &str = "\nTask name: ";
const SENDER_SEPARATOR: &str = "\nSender: ";
const PAYLOAD_SEPARATOR: &str = "\nPayload:\n";
const SUB_AGENT_COMMENTARY_PREFIX: &str = "<subagent_commentary>\n";
const SUB_AGENT_COMMENTARY_SUFFIX: &str = "\n</subagent_commentary>";
const SUB_AGENT_COMMENTARY_TRANSCRIPT_PREFIX: &str = "Agent commentary from `";
const SUB_AGENT_COMMENTARY_TRANSCRIPT_SEPARATOR: &str = "`:\n\n";
const ATTRIBUTED_AGENT_MESSAGE_TRANSCRIPT_PREFIX: &str = "Agent message from `";
const ATTRIBUTED_AGENT_MESSAGE_TRANSCRIPT_SEPARATOR: &str = "`:\n\n";

#[derive(Deserialize)]
struct SubAgentCommentaryEnvelope {
    agent_id: ThreadId,
    message: String,
}

#[derive(Deserialize)]
struct SubAgentNotificationEnvelope {
    agent_id: ThreadId,
    status: AgentStatus,
}

pub(super) fn transcript_text(author: &str, recipient: &str, text: &str) -> String {
    if let Some(SubAgentCommentaryEnvelope { agent_id, message }) =
        sub_agent_commentary_envelope(text)
    {
        return format!(
            "{SUB_AGENT_COMMENTARY_TRANSCRIPT_PREFIX}{agent_id}{SUB_AGENT_COMMENTARY_TRANSCRIPT_SEPARATOR}{message}"
        );
    }
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

pub(super) fn sub_agent_notification(text: &str) -> Option<(ThreadId, AgentStatus)> {
    let body = text
        .strip_prefix("<subagent_notification>\n")?
        .strip_suffix("\n</subagent_notification>")?;
    let notification = serde_json::from_str::<SubAgentNotificationEnvelope>(body).ok()?;
    Some((notification.agent_id, notification.status))
}

/// Parses canonical V1 subagent commentary transcript text into agent identity and message.
pub fn sub_agent_commentary_transcript_parts(text: &str) -> Option<(&str, &str)> {
    text.strip_prefix(SUB_AGENT_COMMENTARY_TRANSCRIPT_PREFIX)?
        .split_once(SUB_AGENT_COMMENTARY_TRANSCRIPT_SEPARATOR)
}

/// Parses canonical attributed V1 agent input into agent identity and message.
pub fn attributed_agent_message_transcript_parts(text: &str) -> Option<(&str, &str)> {
    text.strip_prefix(ATTRIBUTED_AGENT_MESSAGE_TRANSCRIPT_PREFIX)?
        .split_once(ATTRIBUTED_AGENT_MESSAGE_TRANSCRIPT_SEPARATOR)
}

fn sub_agent_commentary_envelope(text: &str) -> Option<SubAgentCommentaryEnvelope> {
    let body = text
        .strip_prefix(SUB_AGENT_COMMENTARY_PREFIX)?
        .strip_suffix(SUB_AGENT_COMMENTARY_SUFFIX)?;
    serde_json::from_str(body).ok()
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
    (task_name == recipient && sender == author).then_some((message_type, payload))
}
