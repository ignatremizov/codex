//! Shared compact text previews for `/agent` controls.

pub(super) const AGENT_PREVIEW_MAX_CHARS: usize = 160;

pub(super) fn compact_agent_preview(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }

    let mut chars = text.chars();
    let mut preview = chars
        .by_ref()
        .take(AGENT_PREVIEW_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    Some(preview)
}
