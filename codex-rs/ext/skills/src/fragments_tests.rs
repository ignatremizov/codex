use codex_extension_api::ContextualUserFragment;
use pretty_assertions::assert_eq;

use super::AvailableSkillsInstructions;
use super::PromotedSkillIdentity;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::catalog_prompt::SkillPromptKind;

#[test]
fn promoted_identity_omits_duplicate_resource() {
    let entry = test_entry("/tmp/example/SKILL.md", "/tmp/example/SKILL.md");
    let identity = PromotedSkillIdentity::from_entry(&entry).expect("identity should be valid");
    let rendered = AvailableSkillsInstructions::from_skill_lines(
        SkillPromptKind::Unaliased,
        Vec::new(),
        Vec::new(),
        vec![identity],
        /*include_skills_usage_instructions*/ false,
    )
    .render();

    assert!(rendered.contains("\"packageHex\""));
    assert!(!rendered.contains("\"resourceHex\""));
}

#[test]
fn promoted_identity_preserves_distinct_resource() {
    let entry = test_entry("skill://package", "skill://package/SKILL.md");
    let identity = PromotedSkillIdentity::from_entry(&entry).expect("identity should be valid");
    let rendered = AvailableSkillsInstructions::from_skill_lines(
        SkillPromptKind::Unaliased,
        Vec::new(),
        Vec::new(),
        vec![identity],
        /*include_skills_usage_instructions*/ false,
    )
    .render();

    assert!(rendered.contains("\"resourceHex\""));
}

#[test]
fn restored_orchestrator_identity_uses_the_current_cloud_authority_kind() {
    let entry = SkillCatalogEntry::new(
        SkillPackageId("skill://demo/example".to_string()),
        SkillAuthority::new(SkillSourceKind::Cloud, "codex_apps"),
        "demo:example",
        "example skill",
        SkillResourceId::new("skill://demo/example/SKILL.md"),
    );
    let identity = PromotedSkillIdentity::from_entry(&entry).expect("valid cloud identity");
    let mut legacy = identity.clone();
    legacy.authority_kind_hex = super::hex_encode(b"orchestrator");
    let rendered = AvailableSkillsInstructions::from_skill_lines(
        SkillPromptKind::Unaliased,
        Vec::new(),
        Vec::new(),
        vec![legacy],
        /*include_skills_usage_instructions*/ false,
    )
    .render();

    let restored = AvailableSkillsInstructions::promoted_from_rendered(&rendered)
        .expect("legacy inventory remains readable");
    assert_eq!(restored, vec![identity]);
    assert!(restored[0].matches_entry(&entry));
}

fn test_entry(package: &str, resource: &str) -> SkillCatalogEntry {
    SkillCatalogEntry::new(
        SkillPackageId(package.to_string()),
        SkillAuthority::new(SkillSourceKind::Host, "host"),
        "example",
        "example skill",
        SkillResourceId::new(resource),
    )
}
