use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::process::Command;

use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use pretty_assertions::assert_eq;

use super::DaemonLaunchOptions;
use crate::Daemon;

#[test]
fn default_children_preserve_backend_and_clear_an_inherited_selection() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let launch = DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::Default,
        AuthCredentialsStoreMode::Keyring,
    )?;
    let mut command = Command::new("codex");
    command.env("CODEX_AUTH_FILE", "auth-other.json");
    command.env("CODEX_HOME", "other-home");
    launch.configure_command(&mut command);
    assert_eq!(
        command.get_envs().collect::<BTreeMap<_, _>>(),
        BTreeMap::from([
            (OsStr::new("CODEX_AUTH_FILE"), None),
            (OsStr::new("CODEX_HOME"), Some(home.path().as_os_str())),
        ]),
    );
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        vec![
            OsStr::new("-c"),
            OsStr::new("cli_auth_credentials_store=\"keyring\"")
        ],
    );
    Ok(())
}

#[test]
fn selected_children_receive_the_captured_absolute_path() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selection = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
    let launch = DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        selection,
        AuthCredentialsStoreMode::File,
    )?;
    let mut command = Command::new("codex");
    command.env("CODEX_AUTH_FILE", "auth-other.json");
    launch.configure_command(&mut command);
    let environment = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        environment,
        BTreeMap::from([
            (
                OsString::from("CODEX_AUTH_FILE"),
                Some(home.path().join("auth-office.json").into_os_string())
            ),
            (
                OsString::from("CODEX_HOME"),
                Some(home.path().as_os_str().to_os_string())
            ),
        ]),
    );
    Ok(())
}

#[test]
fn profiles_share_installation_but_not_lifecycle_state() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let default = DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::Default,
        AuthCredentialsStoreMode::File,
    )?;
    let selected = DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?,
        AuthCredentialsStoreMode::File,
    )?;
    let first = Daemon::from_options(&default)?;
    let second = Daemon::from_options(&selected)?;
    assert_eq!(first.managed_codex_bin, second.managed_codex_bin);
    for (first, second) in [
        (&first.socket_path, &second.socket_path),
        (&first.pid_file, &second.pid_file),
        (&first.update_pid_file, &second.update_pid_file),
        (&first.settings_file, &second.settings_file),
        (&first.operation_lock_file, &second.operation_lock_file),
    ] {
        assert_ne!(first, second);
    }
    Ok(())
}
