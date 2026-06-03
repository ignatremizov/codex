use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;

pub(crate) fn available_skill_names(history: &[ResponseItem]) -> Vec<String> {
    for item in history.iter().rev() {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "developer" {
            continue;
        }
        for content in content {
            let ContentItem::InputText { text } = content else {
                continue;
            };
            if text.contains(SKILLS_INSTRUCTIONS_OPEN_TAG) {
                return available_skill_names_from_inventory(text);
            }
        }
    }
    Vec::new()
}

fn available_skill_names_from_inventory(inventory: &str) -> Vec<String> {
    let Some(body) = inventory
        .split_once(SKILLS_INSTRUCTIONS_OPEN_TAG)
        .and_then(|(_, body)| body.split_once(SKILLS_INSTRUCTIONS_CLOSE_TAG))
        .map(|(body, _)| body)
    else {
        return Vec::new();
    };
    let mut available = false;
    let mut names = Vec::new();
    for line in body.lines().map(str::trim) {
        if line == "### Available skills" {
            available = true;
            continue;
        }
        if available && line.starts_with("### ") {
            break;
        }
        if !available {
            continue;
        }
        let Some(name) = line
            .strip_prefix("- ")
            .and_then(|entry| entry.split_once(':').map(|(name, _)| name.trim()))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        names.push(name.to_string());
    }
    names
}

#[cfg(test)]
#[path = "compact_skills_inventory_tests.rs"]
mod tests;
