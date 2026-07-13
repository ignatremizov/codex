use std::borrow::Cow;

use codex_utils_string::take_bytes_at_char_boundary;

use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;
use crate::fragments::AvailableSkillsInstructions;
use crate::fragments::PromotedSkillIdentity;
use crate::fragments::bounded_promoted_identities;

const MAX_AVAILABLE_SKILLS_BYTES: usize = 8_000;
const MAX_MAIN_PROMPT_BYTES: usize = 8_000;
const MAX_CATALOG_SKILL_DESCRIPTION_CHARS: usize = 1_024;
const TRUNCATED_SKILL_DESCRIPTION_SUFFIX: &str = "...";
pub(crate) const MAX_SKILL_NAME_BYTES: usize = 256;
pub(crate) const MAX_SKILL_PATH_BYTES: usize = 1_024;

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(catalog_entry_count = catalog.entries.len())
)]
pub(crate) fn available_skills_fragment(
    catalog: &SkillCatalog,
    promoted_entries: &[SkillCatalogEntry],
    promoted_identities: &[PromotedSkillIdentity],
    include_skills_usage_instructions: bool,
) -> Option<AvailableSkillsInstructions> {
    let mut total_bytes = 0usize;
    let mut omitted = 0usize;
    let mut skill_lines = Vec::new();

    let entries = promoted_entries.iter().map(|entry| (entry, true)).chain(
        catalog.entries.iter().filter_map(|entry| {
            (entry.enabled
                && entry.prompt_visible
                && !promoted_entries.iter().any(|promoted| {
                    promoted.authority == entry.authority && promoted.id == entry.id
                }))
            .then_some((entry, false))
        }),
    );
    for (entry, promoted) in entries {
        let description = entry
            .short_description
            .as_deref()
            .unwrap_or(entry.description.as_str());
        let description = truncate_catalog_skill_description(description);
        let line = render_skill_line(entry, description.as_ref(), promoted);
        let next_bytes = total_bytes.saturating_add(line.len());
        if next_bytes > MAX_AVAILABLE_SKILLS_BYTES {
            omitted = omitted.saturating_add(1);
            continue;
        }
        total_bytes = next_bytes;
        skill_lines.push(line);
    }

    if skill_lines.is_empty() && promoted_identities.is_empty() {
        return None;
    }
    if omitted > 0 {
        let skill_word = if omitted == 1 { "skill" } else { "skills" };
        skill_lines.push(format!(
            "- {omitted} additional {skill_word} omitted from this bounded skills list."
        ));
    }

    Some(AvailableSkillsInstructions::from_skill_lines(
        skill_lines,
        bounded_promoted_identities(promoted_identities.iter().cloned()),
        include_skills_usage_instructions,
    ))
}

pub(crate) fn truncate_catalog_skill_description(description: &str) -> Cow<'_, str> {
    if description
        .char_indices()
        .nth(MAX_CATALOG_SKILL_DESCRIPTION_CHARS)
        .is_none()
    {
        return Cow::Borrowed(description);
    }

    let prefix_chars = MAX_CATALOG_SKILL_DESCRIPTION_CHARS
        .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count());
    let prefix_end = description
        .char_indices()
        .nth(prefix_chars)
        .map_or(description.len(), |(index, _)| index);
    let mut truncated = description[..prefix_end].to_string();
    truncated.push_str(TRUNCATED_SKILL_DESCRIPTION_SUFFIX);
    Cow::Owned(truncated)
}

fn render_skill_line(entry: &SkillCatalogEntry, description: &str, promoted: bool) -> String {
    let locator_kind = match &entry.authority.kind {
        SkillSourceKind::Host => "file",
        SkillSourceKind::Executor => "environment resource",
        SkillSourceKind::Orchestrator => "orchestrator resource",
        SkillSourceKind::Custom(_) => "custom resource",
    };
    let (name_bytes, path_bytes, description_bytes) = if promoted {
        (128, 192, 128)
    } else {
        (MAX_SKILL_NAME_BYTES, MAX_SKILL_PATH_BYTES, 512)
    };
    let (name, _) = truncate_utf8_to_bytes(entry.name.as_str(), name_bytes);
    let (path, _) = truncate_utf8_to_bytes(entry.rendered_path(), path_bytes);
    let (description, _) = truncate_utf8_to_bytes(description, description_bytes);
    if description.is_empty() {
        format!("- {name}: ({locator_kind}: {path})")
    } else {
        format!("- {name}: {description} ({locator_kind}: {path})")
    }
}

pub(crate) fn truncate_main_prompt_contents(contents: &str) -> (String, bool) {
    truncate_utf8_to_bytes(contents, MAX_MAIN_PROMPT_BYTES)
}

pub(crate) fn truncate_utf8_to_bytes(contents: &str, max_bytes: usize) -> (String, bool) {
    let truncated = take_bytes_at_char_boundary(contents, max_bytes);
    (truncated.to_string(), truncated.len() < contents.len())
}
