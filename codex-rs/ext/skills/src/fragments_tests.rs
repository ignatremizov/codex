use codex_extension_api::ContextualUserFragment;

use super::AvailableSkillsInstructions;
use super::PromotedSkillIdentity;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;

#[test]
fn promoted_identity_omits_duplicate_resource() {
    let entry = test_entry("/tmp/example/SKILL.md", "/tmp/example/SKILL.md");
    let identity = PromotedSkillIdentity::from_entry(&entry).expect("identity should be valid");
    let rendered =
        AvailableSkillsInstructions::from_skill_lines(Vec::new(), vec![identity], false).render();

    assert!(rendered.contains("\"packageHex\""));
    assert!(!rendered.contains("\"resourceHex\""));
}

#[test]
fn promoted_identity_preserves_distinct_resource() {
    let entry = test_entry("skill://package", "skill://package/SKILL.md");
    let identity = PromotedSkillIdentity::from_entry(&entry).expect("identity should be valid");
    let rendered =
        AvailableSkillsInstructions::from_skill_lines(Vec::new(), vec![identity], false).render();

    assert!(rendered.contains("\"resourceHex\""));
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
