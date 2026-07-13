use codex_core_skills::AvailableSkills;
use codex_core_skills::SKILLS_HOW_TO_USE_WITH_ABSOLUTE_PATHS;
use codex_core_skills::SKILLS_HOW_TO_USE_WITH_ALIASES;
use codex_core_skills::render_available_skills_body;
use codex_extension_api::ContextualUserFragment;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use serde::Deserialize;
use serde::Serialize;

use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;

const PROMOTED_SKILLS_OPEN_TAG: &str = "<promoted_skills>";
const PROMOTED_SKILLS_CLOSE_TAG: &str = "</promoted_skills>";
const MAX_PROMOTED_SKILLS: usize = 16;
const MAX_PROMOTED_SKILLS_METADATA_BYTES: usize = 8 * 1024;
const MAX_AUTHORITY_KIND_BYTES: usize = 128;
const MAX_AUTHORITY_ID_BYTES: usize = 256;
const MAX_PACKAGE_ID_BYTES: usize = 256;
const MAX_RESOURCE_ID_BYTES: usize = 512;

/// Bounded provider identity for a skill promoted into the thread's canonical inventory.
///
/// This is inert inventory metadata. It never contains a skill body or model instructions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromotedSkillIdentity {
    authority_kind_hex: String,
    authority_id_hex: String,
    package_hex: String,
    resource_hex: String,
}

impl PromotedSkillIdentity {
    pub(crate) fn from_entry(entry: &SkillCatalogEntry) -> Option<Self> {
        let authority_kind = source_kind_key(&entry.authority.kind);
        (authority_kind.len() <= MAX_AUTHORITY_KIND_BYTES
            && entry.authority.id.len() <= MAX_AUTHORITY_ID_BYTES
            && entry.id.0.len() <= MAX_PACKAGE_ID_BYTES
            && entry.main_prompt.as_str().len() <= MAX_RESOURCE_ID_BYTES)
            .then(|| Self {
                authority_kind_hex: hex_encode(authority_kind.as_bytes()),
                authority_id_hex: hex_encode(entry.authority.id.as_bytes()),
                package_hex: hex_encode(entry.id.0.as_bytes()),
                resource_hex: hex_encode(entry.main_prompt.as_str().as_bytes()),
            })
    }

    pub(crate) fn matches_entry(&self, entry: &SkillCatalogEntry) -> bool {
        let authority_kind = source_kind_key(&entry.authority.kind);
        authority_kind.len() <= MAX_AUTHORITY_KIND_BYTES
            && entry.authority.id.len() <= MAX_AUTHORITY_ID_BYTES
            && entry.id.0.len() <= MAX_PACKAGE_ID_BYTES
            && entry.main_prompt.as_str().len() <= MAX_RESOURCE_ID_BYTES
            && self.authority_kind_hex == hex_encode(authority_kind.as_bytes())
            && self.authority_id_hex == hex_encode(entry.authority.id.as_bytes())
            && self.package_hex == hex_encode(entry.id.0.as_bytes())
            && self.resource_hex == hex_encode(entry.main_prompt.as_str().as_bytes())
    }
}

pub(crate) fn bounded_promoted_identities(
    identities: impl IntoIterator<Item = PromotedSkillIdentity>,
) -> Vec<PromotedSkillIdentity> {
    let mut bounded = Vec::new();
    for identity in identities.into_iter().take(MAX_PROMOTED_SKILLS) {
        bounded.push(identity);
        if !promoted_metadata_is_bounded(&bounded) {
            bounded.pop();
            break;
        }
    }
    bounded
}

pub(crate) fn promoted_metadata_is_bounded(identities: &[PromotedSkillIdentity]) -> bool {
    identities.len() <= MAX_PROMOTED_SKILLS
        && serde_json::to_vec(identities)
            .is_ok_and(|metadata| metadata.len() <= MAX_PROMOTED_SKILLS_METADATA_BYTES)
}

fn source_kind_key(kind: &SkillSourceKind) -> String {
    match kind {
        SkillSourceKind::Host => "host".to_string(),
        SkillSourceKind::Executor => "executor".to_string(),
        SkillSourceKind::Orchestrator => "orchestrator".to_string(),
        SkillSourceKind::Custom(kind) => format!("custom:{kind}"),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AvailableSkillsInstructions {
    skill_root_lines: Vec<String>,
    skill_lines: Vec<String>,
    promoted: Vec<PromotedSkillIdentity>,
}

impl AvailableSkillsInstructions {
    pub(crate) fn from_skill_lines(
        mut skill_lines: Vec<String>,
        promoted: Vec<PromotedSkillIdentity>,
        include_skills_usage_instructions: bool,
    ) -> Self {
        if include_skills_usage_instructions {
            skill_lines.push("### How to use skills".to_string());
            skill_lines.push(SKILLS_HOW_TO_USE_WITH_ABSOLUTE_PATHS.to_string());
        }
        Self {
            skill_root_lines: Vec::new(),
            skill_lines,
            promoted: bounded_promoted_identities(promoted),
        }
    }

    pub(crate) fn from_available_skills(
        available: AvailableSkills,
        include_skills_usage_instructions: bool,
    ) -> Self {
        let mut skill_lines = available.skill_lines;
        if include_skills_usage_instructions {
            skill_lines.push("### How to use skills".to_string());
            let instructions = if available.skill_root_lines.is_empty() {
                SKILLS_HOW_TO_USE_WITH_ABSOLUTE_PATHS
            } else {
                SKILLS_HOW_TO_USE_WITH_ALIASES
            };
            skill_lines.push(instructions.to_string());
        }
        Self {
            skill_root_lines: available.skill_root_lines,
            skill_lines,
            promoted: Vec::new(),
        }
    }

    pub(crate) fn promoted_from_rendered(rendered: &str) -> Option<Vec<PromotedSkillIdentity>> {
        let body = rendered
            .trim()
            .strip_prefix(SKILLS_INSTRUCTIONS_OPEN_TAG)
            .and_then(|body| body.strip_suffix(SKILLS_INSTRUCTIONS_CLOSE_TAG))?;
        let start = body.find(PROMOTED_SKILLS_OPEN_TAG)?;
        let metadata = &body[start + PROMOTED_SKILLS_OPEN_TAG.len()..];
        let end = metadata.find(PROMOTED_SKILLS_CLOSE_TAG)?;
        let metadata = &metadata[..end];
        if metadata.len() > MAX_PROMOTED_SKILLS_METADATA_BYTES {
            return None;
        }
        serde_json::from_str::<Vec<PromotedSkillIdentity>>(metadata)
            .ok()
            .map(bounded_promoted_identities)
    }
}

impl ContextualUserFragment for AvailableSkillsInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (SKILLS_INSTRUCTIONS_OPEN_TAG, SKILLS_INSTRUCTIONS_CLOSE_TAG)
    }

    fn body(&self) -> String {
        let promoted = serde_json::to_string(&self.promoted).unwrap_or_else(|_| "[]".to_string());
        let inventory = render_available_skills_body(&self.skill_root_lines, &self.skill_lines);
        format!(
            "\n{PROMOTED_SKILLS_OPEN_TAG}{promoted}{PROMOTED_SKILLS_CLOSE_TAG}\n\
             When multiple complete skills inventories are present, this latest inventory \
             supersedes earlier inventories.\n{inventory}\n"
        )
    }
}
