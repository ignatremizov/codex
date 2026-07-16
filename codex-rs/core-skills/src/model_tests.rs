use super::compact_model_visible_path;
use codex_utils_absolute_path::AbsolutePathBuf;
use tempfile::tempdir;

#[test]
fn model_visible_skill_path_compacts_the_home_prefix() {
    let home = tempdir().expect("create home");
    let skill_path = AbsolutePathBuf::from_absolute_path(
        home.path().join(".agents/skills/backend-coding/SKILL.md"),
    )
    .expect("absolute skill path");

    assert_eq!(
        compact_model_visible_path(&skill_path, Some(home.path())),
        "~/.agents/skills/backend-coding/SKILL.md"
    );
}
