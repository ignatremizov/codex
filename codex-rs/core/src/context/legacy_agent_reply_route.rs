//! Read-only compatibility for route notices written by earlier fork versions.

use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND;
use serde_json::Value;

pub(crate) struct LegacyAgentReplyRoute;

impl LegacyAgentReplyRoute {
    pub(crate) fn matches_text(text: &str) -> bool {
        let text = text.trim();
        let (start, end) = Self::type_markers();
        text.get(..start.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(start))
            && text
                .get(text.len().saturating_sub(end.len())..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(end))
    }

    pub(crate) fn persistent_agent_id(item: &ResponseItem) -> Option<ThreadId> {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            return None;
        };
        if role != "user" {
            return None;
        }
        let kinds = internal_chat_message_metadata_passthrough
            .as_ref()?
            .content_item_kinds
            .as_ref()?;
        if content.len() != 1 || kinds.len() != 1 {
            return None;
        }
        content.iter().zip(kinds).find_map(|(content, kind)| {
            if kind.0 != PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND {
                return None;
            }
            let codex_protocol::models::ContentItem::InputText { text } = content else {
                return None;
            };
            Self::persistent_agent_id_from_text(text)
        })
    }

    fn persistent_agent_id_from_text(text: &str) -> Option<ThreadId> {
        if !Self::matches_text(text) {
            return None;
        }
        let (start_marker, end_marker) = Self::type_markers();
        let trimmed = text.trim();
        let body = trimmed.get(start_marker.len()..trimmed.len().checked_sub(end_marker.len())?)?;
        let fields = serde_json::from_str::<Value>(body.trim()).ok()?;
        if fields.get("send_input")?.as_str()? != "allowed_until_disabled" {
            return None;
        }
        ThreadId::from_string(fields.get("agent_id")?.as_str()?).ok()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<agent_reply_route>", "</agent_reply_route>")
    }
}
