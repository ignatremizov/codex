use super::*;
use codex_tui::DaemonUpdateSource;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;

#[test]
fn daemon_handoff_uses_selected_executable_and_propagates_failure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let executable = dir.path().join("launching CLI");
    let receipt = dir.path().join("launching CLI.args");
    let launch = codex_app_server_daemon::DaemonLaunchOptions::new(
        dir.path().to_path_buf(),
        codex_login::AuthFileSelection::Default,
        codex_login::AuthCredentialsStoreMode::File,
    )?;
    codex_utils_cargo_bin::write_executable(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n",
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(/*mode*/ 0o700))?;
    for (source, expected) in [
        (
            DaemonUpdateSource::PublicStable,
            "app-server\ndaemon\nupdate\n",
        ),
        (
            DaemonUpdateSource::ThisCli,
            "app-server\ndaemon\nupdate\n--from-cli\n--yes\n",
        ),
    ] {
        run_update_action(
            UpdateAction::Daemon {
                source,
                launch: launch.clone(),
            },
            Some(&executable),
        )?;
        assert_eq!(
            std::fs::read_to_string(&receipt)?,
            format!("-c\ncli_auth_credentials_store=\"file\"\n{expected}")
        );
    }
    codex_utils_cargo_bin::write_executable(&executable, "#!/bin/sh\nexit 7\n")?;
    let error = run_update_action(
        UpdateAction::Daemon {
            source: DaemonUpdateSource::ThisCli,
            launch,
        },
        Some(&executable),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Daemon update failed"));
    Ok(())
}
