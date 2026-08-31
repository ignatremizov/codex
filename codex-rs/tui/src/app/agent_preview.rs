//! Shared text previews for `/agent` controls.

pub(super) const AGENT_PREVIEW_MAX_CHARS: usize = 160;
pub(super) const AGENT_DETAIL_PREVIEW_MAX_CHARS: usize = 1_024;

pub(super) fn compact_agent_preview(text: &str) -> Option<String> {
    agent_preview(text, AGENT_PREVIEW_MAX_CHARS)
}

pub(super) fn detailed_agent_preview(text: &str) -> Option<String> {
    agent_preview(text, AGENT_DETAIL_PREVIEW_MAX_CHARS)
}

fn agent_preview(text: &str, max_chars: usize) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }

    let mut chars = text.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    Some(preview)
}

#[cfg(test)]
#[path = "agent_preview_tests.rs"]
mod tests;
