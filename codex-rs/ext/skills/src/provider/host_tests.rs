use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_exec_server::LOCAL_FS;
use codex_extension_api::ContextualUserFragment;
use codex_protocol::protocol::SkillScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;

use super::catalog_from_outcome;
use super::compact_model_visible_path;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::fragments::PromotedSkillIdentity;
use crate::loader::HostSkillRoot;
use crate::loader::load_and_merge_host_skill_roots;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::SkillMetadataBudget;
use crate::render::render_available_skills;

fn expected_model_display_path(path: &AbsolutePathBuf) -> String {
    let rendered = dirs::home_dir()
        .and_then(|home| path.as_path().strip_prefix(home).ok())
        .map(|relative| format!("~/{}", relative.to_string_lossy()))
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    rendered.replace('\\', "/")
}

#[test]
fn model_visible_host_path_contracts_home_and_normalizes_separators() {
    let home = tempfile::tempdir().expect("create home");
    let native_skill_path = AbsolutePathBuf::from_absolute_path(
        home.path().join(".agents/skills/backend-coding/SKILL.md"),
    )
    .expect("absolute native skill path");
    let backslash_skill_path = AbsolutePathBuf::from_absolute_path(
        home.path().join(r".agents\skills\backend-coding\SKILL.md"),
    )
    .expect("absolute backslash skill path");

    assert_eq!(
        (
            compact_model_visible_path(&native_skill_path, Some(home.path())),
            compact_model_visible_path(&backslash_skill_path, Some(home.path())),
        ),
        (
            "~/.agents/skills/backend-coding/SKILL.md".to_string(),
            "~/.agents/skills/backend-coding/SKILL.md".to_string(),
        )
    );
}

#[test]
fn model_visible_host_path_handles_home_boundaries_and_missing_home() {
    let fixture = tempfile::tempdir().expect("create path fixture");
    let home = fixture.path().join("home");
    let home_relative = AbsolutePathBuf::from_absolute_path(home.join("skills/demo/SKILL.md"))
        .expect("home-relative skill path");
    let outside_home =
        AbsolutePathBuf::from_absolute_path(fixture.path().join("home-other/skill/SKILL.md"))
            .expect("outside-home skill path");
    let unrelated = AbsolutePathBuf::from_absolute_path(fixture.path().join("elsewhere/SKILL.md"))
        .expect("unrelated skill path");

    assert_eq!(
        (
            compact_model_visible_path(&home_relative, Some(&home)),
            compact_model_visible_path(&outside_home, Some(&home)),
            compact_model_visible_path(&unrelated, /*home_dir*/ None),
        ),
        (
            "~/skills/demo/SKILL.md".to_string(),
            outside_home.to_string_lossy().replace('\\', "/"),
            unrelated.to_string_lossy().replace('\\', "/"),
        )
    );
}

#[tokio::test]
async fn host_catalog_entries_carry_their_render_metadata() -> Result<(), Box<dyn std::error::Error>>
{
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = std::env::temp_dir().join(format!(
        "codex-skills-extension-host-provider-{}-{unique}",
        std::process::id()
    ));
    let skill_path = root.join("demo").join("SKILL.md");
    std::fs::create_dir_all(
        skill_path
            .parent()
            .ok_or("skill path should have a parent")?,
    )?;
    std::fs::write(
        &skill_path,
        "---\nname: demo\ndescription: Demo skill.\n---\n# Demo\n",
    )?;
    let root = AbsolutePathBuf::try_from(std::fs::canonicalize(root)?)?;
    let outcome = load_and_merge_host_skill_roots(
        vec![HostSkillRoot::host(
            root.clone(),
            SkillScope::User,
            Arc::clone(&LOCAL_FS),
        )],
        &Semaphore::new(/*permits*/ 1),
        /*restriction_product*/ None,
        /*plugin_skill_snapshots*/ None,
    )
    .await;

    let catalog = catalog_from_outcome(&outcome);

    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(
        (
            catalog.entries[0].alias_root(),
            catalog.entries[0].prompt_scope(),
        ),
        (
            Some(root.to_string_lossy().replace('\\', "/").as_str()),
            Some(SkillScope::User),
        )
    );

    std::fs::remove_dir_all(root.as_path())?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn host_catalog_preserves_symlinked_skill_discovery_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let source = tempfile::tempdir()?;
    let source_skill_dir = source.path().join("linked-skill");
    std::fs::create_dir_all(&source_skill_dir)?;
    std::fs::write(
        source_skill_dir.join("SKILL.md"),
        "---\nname: linked-skill\ndescription: Linked skill.\n---\n# Linked skill\n",
    )?;
    std::os::unix::fs::symlink(&source_skill_dir, root.path().join("linked-skill"))?;

    let root = AbsolutePathBuf::try_from(std::fs::canonicalize(root.path())?)?;
    let outcome = load_and_merge_host_skill_roots(
        vec![HostSkillRoot::host(
            root.clone(),
            SkillScope::User,
            Arc::clone(&LOCAL_FS),
        )],
        &Semaphore::new(/*permits*/ 1),
        /*restriction_product*/ None,
        /*plugin_skill_snapshots*/ None,
    )
    .await;
    let catalog = catalog_from_outcome(&outcome);
    let canonical_path = std::fs::canonicalize(source_skill_dir.join("SKILL.md"))?;
    let discovery_path = root.join("linked-skill/SKILL.md");
    let canonical_path = AbsolutePathBuf::try_from(canonical_path)?;
    let model_visible_discovery_path = expected_model_display_path(&discovery_path);

    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(
        (
            catalog.entries[0].id.0.as_str(),
            catalog.entries[0].main_prompt.as_str(),
            catalog.entries[0].authority.clone(),
            catalog.entries[0].display_path.as_deref(),
            catalog.entries[0].alias_root(),
        ),
        (
            canonical_path.to_string_lossy().as_ref(),
            canonical_path.to_string_lossy().as_ref(),
            SkillAuthority::new(SkillSourceKind::Host, "host"),
            Some(model_visible_discovery_path.as_str()),
            Some(root.to_string_lossy().as_ref()),
        )
    );
    let expected_entry = SkillCatalogEntry::new(
        SkillPackageId(canonical_path.to_string_lossy().into_owned()),
        SkillAuthority::new(SkillSourceKind::Host, "host"),
        "linked-skill",
        "Linked skill.",
        SkillResourceId::new(canonical_path.to_string_lossy().into_owned()),
    );
    assert_eq!(
        PromotedSkillIdentity::from_entry(&catalog.entries[0]),
        PromotedSkillIdentity::from_entry(&expected_entry),
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn host_catalog_preserves_symlinked_root_discovery_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let discovery_parent = tempfile::tempdir()?;
    let source = tempfile::tempdir()?;
    let source_skill_dir = source.path().join("linked-skill");
    std::fs::create_dir_all(&source_skill_dir)?;
    std::fs::write(
        source_skill_dir.join("SKILL.md"),
        "---\nname: linked-skill\ndescription: Linked skill.\n---\n# Linked skill\n",
    )?;
    let discovered_root_path = discovery_parent.path().join("skills");
    std::os::unix::fs::symlink(source.path(), &discovered_root_path)?;

    let discovered_root = AbsolutePathBuf::try_from(discovered_root_path)?;
    let outcome = load_and_merge_host_skill_roots(
        vec![HostSkillRoot::host(
            discovered_root.clone(),
            SkillScope::User,
            Arc::clone(&LOCAL_FS),
        )],
        &Semaphore::new(/*permits*/ 1),
        /*restriction_product*/ None,
        /*plugin_skill_snapshots*/ None,
    )
    .await;
    let catalog = catalog_from_outcome(&outcome);
    let canonical_path =
        AbsolutePathBuf::try_from(std::fs::canonicalize(source_skill_dir.join("SKILL.md"))?)?;
    let discovery_path = discovered_root.join("linked-skill/SKILL.md");
    let canonical_root = AbsolutePathBuf::try_from(std::fs::canonicalize(source.path())?)?;
    let model_visible_discovery_path = expected_model_display_path(&discovery_path);
    let model_visible_canonical_path = expected_model_display_path(&canonical_path);
    let canonical_root_string = canonical_root.to_string_lossy().replace('\\', "/");

    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(
        (
            catalog.entries[0].id.0.as_str(),
            catalog.entries[0].main_prompt.as_str(),
            catalog.entries[0].authority.clone(),
            catalog.entries[0].display_path.as_deref(),
            catalog.entries[0].alias_root(),
        ),
        (
            canonical_path.to_string_lossy().as_ref(),
            canonical_path.to_string_lossy().as_ref(),
            SkillAuthority::new(SkillSourceKind::Host, "host"),
            Some(model_visible_discovery_path.as_str()),
            Some(canonical_root_string.as_str()),
        )
    );
    let expected_entry = SkillCatalogEntry::new(
        SkillPackageId(canonical_path.to_string_lossy().into_owned()),
        SkillAuthority::new(SkillSourceKind::Host, "host"),
        "linked-skill",
        "Linked skill.",
        SkillResourceId::new(canonical_path.to_string_lossy().into_owned()),
    );
    assert_eq!(
        PromotedSkillIdentity::from_entry(&catalog.entries[0]),
        PromotedSkillIdentity::from_entry(&expected_entry),
    );
    let rendered = render_available_skills(
        &catalog,
        SkillCatalogRenderPolicy::CoreCompatible,
        SkillMetadataBudget::Characters(usize::MAX),
        /*include_skills_usage_instructions*/ false,
    )
    .expect("host catalog should render")
    .into_fragment(/*include_skills_usage_instructions*/ false)
    .expect("host catalog should produce an inventory")
    .body();
    assert!(
        rendered.contains(&format!("(file: {model_visible_discovery_path})")),
        "the model-visible inventory should retain the symlinked root"
    );
    assert!(
        !rendered.contains(&format!("(file: {model_visible_canonical_path})")),
        "the model-visible inventory should not expose the canonical storage path"
    );

    Ok(())
}
