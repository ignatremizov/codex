const MESSAGE_TYPE_PREFIX: &str = "Message Type: ";
const TASK_NAME_SEPARATOR: &str = "\nTask name: ";
const SENDER_SEPARATOR: &str = "\nSender: ";
const PAYLOAD_SEPARATOR: &str = "\nPayload:\n";

pub(super) fn transcript_text(author: &str, recipient: &str, text: &str) -> String {
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
    (task_name == recipient && sender == author).then_some((message_type, payload))
}
