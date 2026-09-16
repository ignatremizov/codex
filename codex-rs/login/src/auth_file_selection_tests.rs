use super::AuthFileSelection;
use codex_config::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;

#[test]
fn relative_selection_is_resolved_under_home_before_file_exists() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
    assert_eq!(
        selected,
        AuthFileSelection::Selected(AbsolutePathBuf::from_absolute_path_checked(
            home.path().join("auth-office.json")
        )?)
    );
    std::fs::write(home.path().join("auth-office.json"), "{}")?;
    assert_eq!(
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?,
        selected
    );
    Ok(())
}

#[test]
fn explicit_absolute_selection_is_not_the_default_backend() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("auth.json");
    let selection = AuthFileSelection::resolve(home.path(), Some(path.as_os_str()))?;
    assert_eq!(
        selection,
        AuthFileSelection::Selected(AbsolutePathBuf::from_absolute_path_checked(path)?)
    );
    assert_eq!(
        AuthFileSelection::resolve(home.path(), /*selector*/ None)?,
        AuthFileSelection::Default
    );
    Ok(())
}

#[test]
fn invalid_selectors_are_rejected() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    for selector in [
        "",
        ".",
        "..",
        "../auth.json",
        "nested/auth.json",
        "auth\n.json",
    ] {
        assert_eq!(
            AuthFileSelection::resolve(home.path(), Some(OsStr::new(selector)))
                .expect_err("invalid selector must fail")
                .kind(),
            std::io::ErrorKind::InvalidInput,
            "{selector:?}"
        );
    }
    assert_eq!(
        AuthFileSelection::resolve(home.path(), Some(home.path().as_os_str()))
            .expect_err("directory must fail")
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn credential_symlink_is_rejected_without_following_it() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let link = home.path().join("auth-office.json");
    std::os::unix::fs::symlink(home.path().join("auth.json"), &link)?;
    assert_eq!(
        AuthFileSelection::resolve(home.path(), Some(link.as_os_str()))
            .expect_err("credential symlink must fail")
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    Ok(())
}
