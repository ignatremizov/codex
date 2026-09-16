use super::run_update_action;
use codex_app_server_daemon::DaemonLaunchOptions;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use codex_tui::DaemonUpdateSource;
use codex_tui::UpdateAction;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;

#[test]
fn daemon_menu_handoff_passes_captured_home_selection_and_backend_to_the_child()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let executable = home.path().join("record-update");
    let output = home.path().join("handoff.txt");
    let output_path = output
        .to_str()
        .expect("UTF-8 temporary path")
        .replace('\'', "'\\''");
    codex_utils_cargo_bin::write_executable(
        &executable,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\" \"${{CODEX_AUTH_FILE-unset}}\" \"$@\" > '{output_path}'\n"
        ),
    )?;
    let selected = home.path().join("auth-office.json");
    for (selection, mode, expected_file, expected_mode) in [
        (
            AuthFileSelection::Default,
            AuthCredentialsStoreMode::Keyring,
            "unset".to_string(),
            "keyring",
        ),
        (
            AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?,
            AuthCredentialsStoreMode::File,
            selected.display().to_string(),
            "file",
        ),
    ] {
        run_update_action(
            UpdateAction::Daemon {
                source: DaemonUpdateSource::PublicStable,
                launch: DaemonLaunchOptions::new(home.path().to_path_buf(), selection, mode)?,
            },
            Some(&executable),
        )?;
        assert_eq!(
            std::fs::read_to_string(&output)?,
            format!(
                "{}\n{expected_file}\n-c\ncli_auth_credentials_store=\"{expected_mode}\"\napp-server\ndaemon\nupdate\n",
                home.path().display()
            )
        );
    }
    Ok(())
}
