use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::process::Command;

#[cfg(unix)]
use anyhow::Context as _;
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
    let first_recovery = first.recovery_file()?;
    let second_recovery = second.recovery_file()?;
    for (first, second) in [
        (&first.socket_path, &second.socket_path),
        (&first.pid_file, &second.pid_file),
        (&first.update_pid_file, &second.update_pid_file),
        (&first.settings_file, &second.settings_file),
        (&first.operation_lock_file, &second.operation_lock_file),
        (&first_recovery, &second_recovery),
    ] {
        assert_ne!(first, second);
    }
    Ok(())
}

#[test]
fn profile_restart_cleanup_preserves_other_profile_and_legacy_snapshots() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let first = Daemon::from_options(&DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::Default,
        AuthCredentialsStoreMode::File,
    )?)?;
    let second = Daemon::from_options(&DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?,
        AuthCredentialsStoreMode::File,
    )?)?;
    let first_path = first.recovery_file()?;
    let second_path = second.recovery_file()?;
    let legacy_path = codex_app_server_transport::daemon_recovery_file_path(home.path());
    for path in [&first_path, &second_path, &legacy_path] {
        std::fs::create_dir_all(path.parent().expect("recovery directory"))?;
        std::fs::write(path, b"[\"retained-thread\"]")?;
    }
    crate::thread_recovery::discard_pending(&first)?;
    assert!(!first_path.exists());
    assert_eq!(std::fs::read(&second_path)?, b"[\"retained-thread\"]");
    assert_eq!(std::fs::read(&legacy_path)?, b"[\"retained-thread\"]");
    crate::thread_recovery::discard_pending(&second)?;
    assert!(!second_path.exists());
    assert_eq!(std::fs::read(&legacy_path)?, b"[\"retained-thread\"]");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn profile_updater_sockets_bind_alongside_server_sockets() -> anyhow::Result<()> {
    let home = tempfile::Builder::new().tempdir_in("/tmp")?;
    let mut listeners = Vec::new();
    for selection in [
        AuthFileSelection::Default,
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?,
    ] {
        let launch = DaemonLaunchOptions::new(
            home.path().to_path_buf(),
            selection,
            AuthCredentialsStoreMode::File,
        )?;
        let daemon = Daemon::from_options(&launch)?;
        let updater_socket = daemon.manual_update_socket_path();
        for socket in [&daemon.socket_path, &updater_socket] {
            codex_uds::prepare_private_socket_directory(
                socket.parent().context("socket directory")?,
            )
            .await?;
            // All four listeners coexist: profiles and server/updater roles stay separate.
            listeners.push(codex_uds::UnixListener::bind(socket).await?);
            let _connection = codex_uds::UnixStream::connect(socket).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn first_operation_creates_private_root_and_separate_profile_directories()
-> anyhow::Result<()> {
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
    let root = home.path().join(crate::STATE_DIR_NAME);
    assert!(!root.exists());
    let (first_lock, second_lock) = tokio::join!(
        first.open_operation_lock_file(),
        second.open_operation_lock_file(),
    );
    let _first_lock = first_lock?;
    let _second_lock = second_lock?;
    assert_ne!(first.operation_lock_file, second.operation_lock_file);
    for file in [&first.operation_lock_file, &second.operation_lock_file] {
        assert!(file.is_file());
        let profile_dir = file.parent().expect("profile directory");
        assert_eq!(profile_dir.parent(), Some(root.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [root.as_path(), profile_dir] {
                assert_eq!(
                    std::fs::metadata(directory)?.permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn operation_lock_rejects_linked_root_and_profile_directories() -> anyhow::Result<()> {
    for linked_root in [true, false] {
        let home = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let launch = DaemonLaunchOptions::new(
            home.path().to_path_buf(),
            AuthFileSelection::Default,
            AuthCredentialsStoreMode::File,
        )?;
        let daemon = Daemon::from_options(&launch)?;
        let root = home.path().join(crate::STATE_DIR_NAME);
        let destination = if linked_root {
            root
        } else {
            std::fs::create_dir(&root)?;
            daemon
                .operation_lock_file
                .parent()
                .expect("profile directory")
                .to_path_buf()
        };
        std::os::unix::fs::symlink(outside.path(), destination)?;
        assert!(daemon.open_operation_lock_file().await.is_err());
        assert_eq!(std::fs::read_dir(outside.path())?.count(), 0);
    }
    Ok(())
}
