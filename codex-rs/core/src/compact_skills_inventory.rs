//! Reports the skill catalog that survived compaction, without rediscovering skills.

use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;

pub(crate) fn available_skill_names(history: &[ResponseItemEnvelope]) -> Vec<String> {
    let latest = history.iter().rev().find_map(|envelope| {
        let ResponseItem::Message { role, content, .. } = &envelope.item else {
            return None;
        };
        if role != "developer" {
            return None;
        }
        content.iter().rev().find_map(|part| match part {
            ContentItem::InputText { text } if text.contains(SKILLS_INSTRUCTIONS_OPEN_TAG) => {
                Some(text.as_str())
            }
            _ => None,
        })
    });
    let Some(body) = latest
        .and_then(|text| text.rsplit_once(SKILLS_INSTRUCTIONS_OPEN_TAG))
        .and_then(|(_, body)| body.split_once(SKILLS_INSTRUCTIONS_CLOSE_TAG))
        .map(|(body, _)| body)
    else {
        return Vec::new();
    };
    let mut lines = body.lines().map(str::trim);
    if !lines.any(|line| line == "### Available skills") {
        return Vec::new();
    }
    lines
        .take_while(|line| !line.starts_with("## "))
        .take_while(|line| !line.starts_with("### "))
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
#[path = "compact_skills_inventory_tests.rs"]
mod tests;
