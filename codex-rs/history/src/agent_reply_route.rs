//! Recognition of harness-owned singleton context, never live reply authority.

use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND;

use crate::ResponseItemEnvelope;

/// Returns the source of a complete, non-client-authored persistent route envelope.
///
/// Mixed user content is deliberately excluded: retention must not promote an enclosing prompt
/// merely because it contains a route-shaped fragment. This evidence only controls history
/// retention; cold history cannot authorize input or restore a live grant.
pub fn persistent_agent_reply_route_source(envelope: &ResponseItemEnvelope) -> Option<ThreadId> {
    if envelope
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.client_authored)
    {
        return None;
    }
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return None;
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        return None;
    };
    let kinds = internal_chat_message_metadata_passthrough
        .as_ref()?
        .content_item_kinds
        .as_ref()?;
    if role != "user" || kinds.len() != 1 || kinds[0].0 != PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND
    {
        return None;
    }
    let body = text
        .trim()
        .strip_prefix("<agent_reply_route>")?
        .strip_suffix("</agent_reply_route>")?;
    let fields: serde_json::Value = serde_json::from_str(body).ok()?;
    if fields.get("send_input")?.as_str()? != "allowed_until_disabled" {
        return None;
    }
    ThreadId::from_string(fields.get("agent_id")?.as_str()?).ok()
}
