use super::ThreadItemPayload;
use super::UserInput;

/// Render trusted inbound agent input for text-only consumers, without exposing media data.
///
/// Canonical identities come from attribution, never from payload text. Text input is kept
/// verbatim, including whitespace and Markdown; non-text items retain their place as markers.
/// This is presentation only and must not be used as model input or an assistant final response.
pub fn attributed_agent_input_text(item: &ThreadItemPayload) -> Option<String> {
    let ThreadItemPayload::AgentMessage {
        id,
        attribution: Some(attribution),
        input,
        text,
        ..
    } = item
    else {
        return None;
    };
    // Response provenance shares the identity snapshot, but remains assistant output.
    if codex_protocol::protocol::sub_agent_completion_status_from_response_item_id(id).is_some()
        || codex_protocol::protocol::agent_delivery_receipt_from_response_item_id(id).is_some()
    {
        return None;
    }
    let payload = match input {
        Some(input) => input
            .iter()
            .map(|input| match input {
                UserInput::Text { text, .. } => text.clone(),
                UserInput::Image { .. } | UserInput::LocalImage { .. } => "[image]".into(),
                UserInput::Audio { .. } | UserInput::LocalAudio { .. } => "[audio]".into(),
                UserInput::Skill { name, .. } => format!("[skill: {name}]"),
                UserInput::Mention { name, .. } => format!("[mention: {name}]"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => text.clone(),
    };
    Some(format!(
        "Agent message from `{}` to `{}`:\n\n{payload}",
        attribution.sender.thread_id, attribution.recipient.thread_id
    ))
}

#[cfg(test)]
#[path = "agent_input_text_tests.rs"]
mod tests;
