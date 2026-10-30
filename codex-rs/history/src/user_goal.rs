//! Recognize host-annotated goal instructions that have no visible prompt presentation.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub const USER_GOAL_CONTENT_KIND: &str = "user.goal";
pub const USER_GOAL_OMITTED_CONTENT_KIND: &str = "user.goal.omitted";

/// Read only a typed user goal update, never a matching text wrapper alone.
pub fn user_goal_update_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = item
    else {
        return None;
    };
    let [kind] = metadata.content_item_kinds.as_deref()? else {
        return None;
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        return None;
    };
    (role == "user"
        && matches!(
            kind.0.as_str(),
            USER_GOAL_CONTENT_KIND | USER_GOAL_OMITTED_CONTENT_KIND
        ))
    .then_some(text)
}
