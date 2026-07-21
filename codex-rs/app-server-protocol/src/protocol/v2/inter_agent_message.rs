const FINAL_ANSWER_PREFIX: &str = "Message Type: FINAL_ANSWER\nTask name: ";
const SENDER_SEPARATOR: &str = "\nSender: ";
const PAYLOAD_SEPARATOR: &str = "\nPayload:\n";

pub(super) fn transcript_text(author: &str, recipient: &str, text: &str) -> String {
    match final_answer_payload(author, recipient, text) {
        Some(payload) => format!("Agent final answer from `{author}`:\n\n{payload}"),
        None => format!("Agent message from `{author}`:\n\n{text}"),
    }
}

fn final_answer_payload<'a>(author: &str, recipient: &str, text: &'a str) -> Option<&'a str> {
    let envelope = text.strip_prefix(FINAL_ANSWER_PREFIX)?;
    let (task_name, envelope) = envelope.split_once(SENDER_SEPARATOR)?;
    let (sender, payload) = envelope.split_once(PAYLOAD_SEPARATOR)?;
    (task_name == recipient && sender == author).then_some(payload)
}
