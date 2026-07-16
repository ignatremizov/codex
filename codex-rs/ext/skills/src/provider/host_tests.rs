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
use crate::loader::HostSkillRoot;
use crate::loader::load_and_merge_host_skill_roots;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::SkillMetadataBudget;
use crate::render::render_available_skills;

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
    let canonical_path = std::fs::canonicalize(source_skill_dir.join("SKILL.md"))?;
    let discovery_path = discovered_root.join("linked-skill/SKILL.md");
    let model_visible_discovery_path = discovery_path.to_string_lossy().replace('\\', "/");

    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(
        (
            catalog.entries[0].id.0.as_str(),
            catalog.entries[0].main_prompt.as_str(),
            catalog.entries[0].display_path.as_deref(),
        ),
        (
            canonical_path.to_string_lossy().as_ref(),
            canonical_path.to_string_lossy().as_ref(),
            Some(model_visible_discovery_path.as_str()),
        )
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
        !rendered.contains(&format!(
            "(file: {})",
            canonical_path.to_string_lossy().replace('\\', "/")
        )),
        "the model-visible inventory should not expose the canonical storage path"
    );

    Ok(())
}
