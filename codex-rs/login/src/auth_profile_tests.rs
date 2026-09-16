use std::ffi::OsStr;

use pretty_assertions::assert_eq;

use crate::AuthCredentialsStoreMode;
use crate::AuthFileSelection;

#[test]
fn explicit_file_profiles_ignore_persistent_backend_policy() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
    let identity = selected.profile_identity(home.path(), AuthCredentialsStoreMode::File);
    assert_eq!(
        identity,
        selected.profile_identity(home.path(), AuthCredentialsStoreMode::Keyring)
    );
    assert_eq!(identity.display_label, "codex-office");
    Ok(())
}

#[test]
fn default_policy_and_explicit_selection_remain_distinct() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth.json")))?;
    let default = AuthFileSelection::Default;
    assert_ne!(
        default.profile_identity(home.path(), AuthCredentialsStoreMode::File),
        selected.profile_identity(home.path(), AuthCredentialsStoreMode::File)
    );
    assert_ne!(
        default.profile_identity(home.path(), AuthCredentialsStoreMode::Auto),
        default.profile_identity(home.path(), AuthCredentialsStoreMode::Keyring)
    );
    Ok(())
}

#[test]
fn profile_ids_distinguish_paths_without_exposing_email_labels() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let first =
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("person@example.com.json")))?;
    let second =
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("other@example.com.json")))?;
    let first = first.profile_identity(home.path(), AuthCredentialsStoreMode::File);
    let second = second.profile_identity(home.path(), AuthCredentialsStoreMode::File);
    assert_ne!(first.profile_opaque_id, second.profile_opaque_id);
    assert_eq!(first.display_label, "codex-profile");
    assert_eq!(second.display_label, "codex-profile");
    Ok(())
}

#[test]
fn ephemeral_selection_is_not_persistent_file_identity() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
    assert_ne!(
        selected.profile_identity(home.path(), AuthCredentialsStoreMode::Ephemeral),
        selected.profile_identity(home.path(), AuthCredentialsStoreMode::File)
    );
    Ok(())
}
