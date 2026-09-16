use anyhow::Result;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use codex_login::AuthProfileIdentity;
use codex_uds::UnixListener;
use pretty_assertions::assert_eq;

use super::DaemonLaunchOptions;
use super::DaemonProfileLookup;
use super::candidates;
use super::resolve_existing_launch;
use crate::Daemon;
use crate::client;

fn short_home() -> std::io::Result<tempfile::TempDir> {
    // macOS's default temp directory can exceed the Unix socket path limit.
    tempfile::Builder::new().prefix("codex-").tempdir_in("/tmp")
}

fn launch(home: &std::path::Path, mode: AuthCredentialsStoreMode) -> DaemonLaunchOptions {
    DaemonLaunchOptions::new(home.to_path_buf(), AuthFileSelection::Default, mode)
        .expect("launch options")
}

async fn serve_profile(
    launch: &DaemonLaunchOptions,
    reported_profile: AuthProfileIdentity,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let socket = launch.socket_path()?;
    tokio::fs::create_dir_all(socket.parent().expect("socket parent")).await?;
    let mut listener = UnixListener::bind(&socket).await?;
    let home = launch.codex_home().to_path_buf();
    Ok(tokio::spawn(async move {
        let stream = listener.accept().await?;
        let mut websocket = tokio_tungstenite::accept_async(stream).await?;
        let JSONRPCMessage::Request(initialize) = client::read_message(&mut websocket).await?
        else {
            anyhow::bail!("expected initialize");
        };
        client::send_message(
            &mut websocket,
            &JSONRPCMessage::Response(JSONRPCResponse {
                id: initialize.id,
                result: serde_json::json!({
                    "userAgent": "codex_app_server/1.2.3",
                    "codexHome": home,
                    "platformFamily": "unix",
                    "platformOs": std::env::consts::OS,
                }),
            }),
        )
        .await?;
        let JSONRPCMessage::Notification(initialized) =
            client::read_message(&mut websocket).await?
        else {
            anyhow::bail!("expected initialized");
        };
        assert_eq!(initialized.method, "initialized");
        let JSONRPCMessage::Request(profile) = client::read_message(&mut websocket).await? else {
            anyhow::bail!("expected server/read");
        };
        assert_eq!(profile.method, "server/read");
        client::send_message(
            &mut websocket,
            &JSONRPCMessage::Response(JSONRPCResponse {
                id: profile.id,
                result: serde_json::json!({
                    "authProfile": {
                        "profileOpaqueId": reported_profile.profile_opaque_id,
                        "displayLabel": reported_profile.display_label,
                    },
                }),
            }),
        )
        .await
    }))
}

#[tokio::test]
async fn offline_lookup_recovers_a_running_cloud_required_backend() -> Result<()> {
    let home = short_home()?;
    let nominal = launch(home.path(), AuthCredentialsStoreMode::File);
    let running = launch(home.path(), AuthCredentialsStoreMode::Keyring);
    let server = serve_profile(&running, running.auth_profile().clone()).await?;
    let resolved = resolve_existing_launch(nominal, DaemonProfileLookup::AnyBackend).await?;
    assert_eq!(resolved.auth_profile(), running.auth_profile());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn offline_lookup_does_not_adopt_socket_with_mismatched_metadata() -> Result<()> {
    let home = short_home()?;
    let nominal = launch(home.path(), AuthCredentialsStoreMode::File);
    let other = launch(home.path(), AuthCredentialsStoreMode::Keyring);
    let server = serve_profile(&other, nominal.auth_profile().clone()).await?;
    let resolved =
        resolve_existing_launch(nominal.clone(), DaemonProfileLookup::AnyBackend).await?;
    assert_eq!(resolved.auth_profile(), nominal.auth_profile());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn offline_lookup_requires_explicit_backend_when_multiple_profiles_are_live() -> Result<()> {
    let home = short_home()?;
    let first = launch(home.path(), AuthCredentialsStoreMode::File);
    let second = launch(home.path(), AuthCredentialsStoreMode::Keyring);
    let first_server = serve_profile(&first, first.auth_profile().clone()).await?;
    let second_server = serve_profile(&second, second.auth_profile().clone()).await?;
    let error = resolve_existing_launch(first, DaemonProfileLookup::AnyBackend)
        .await
        .expect_err("multiple profiles must be ambiguous");
    assert!(error.to_string().contains("cli_auth_credentials_store"));
    first_server.await??;
    second_server.await??;
    Ok(())
}

#[tokio::test]
async fn explicit_backend_does_not_fall_back_to_another_identity() -> Result<()> {
    let home = short_home()?;
    let nominal = launch(home.path(), AuthCredentialsStoreMode::File);
    let expected = launch(home.path(), AuthCredentialsStoreMode::Keyring);
    let resolved = resolve_existing_launch(
        nominal,
        DaemonProfileLookup::ExactBackend(AuthCredentialsStoreMode::Keyring),
    )
    .await?;
    assert_eq!(resolved.auth_profile(), expected.auth_profile());
    Ok(())
}

#[tokio::test]
async fn stale_pid_fingerprint_does_not_select_a_profile() -> Result<()> {
    let home = short_home()?;
    let stale = launch(home.path(), AuthCredentialsStoreMode::File);
    let daemon = Daemon::from_options(&stale)?;
    tokio::fs::create_dir_all(daemon.pid_file.parent().expect("pid parent")).await?;
    tokio::fs::write(
        &daemon.pid_file,
        serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(),
            "processStartTime": "not the current process start time",
        }))?,
    )
    .await?;
    let nominal = launch(home.path(), AuthCredentialsStoreMode::Keyring);
    let resolved =
        resolve_existing_launch(nominal.clone(), DaemonProfileLookup::AnyBackend).await?;
    assert_eq!(resolved.auth_profile(), nominal.auth_profile());
    assert!(!tokio::fs::try_exists(&daemon.pid_file).await?);
    Ok(())
}

#[tokio::test]
async fn verified_pid_selects_an_unresponsive_profile_without_socket_proof() -> Result<()> {
    let home = short_home()?;
    let running = launch(home.path(), AuthCredentialsStoreMode::Keyring);
    let daemon = Daemon::from_options(&running)?;
    let pid = std::process::id();
    let output = tokio::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .await?;
    assert!(output.status.success());
    let start_time = String::from_utf8(output.stdout)?;
    tokio::fs::create_dir_all(daemon.pid_file.parent().expect("pid parent")).await?;
    tokio::fs::write(
        &daemon.pid_file,
        serde_json::to_vec(&serde_json::json!({
            "pid": pid,
            "processStartTime": start_time.trim(),
        }))?,
    )
    .await?;
    let nominal = launch(home.path(), AuthCredentialsStoreMode::File);
    let resolved = resolve_existing_launch(nominal, DaemonProfileLookup::AnyBackend).await?;
    assert_eq!(resolved.auth_profile(), running.auth_profile());
    // Lookup never sends a termination signal.
    assert!(tokio::fs::try_exists(&daemon.pid_file).await?);
    Ok(())
}

#[test]
fn selected_file_persistent_backend_aliases_are_deduplicated() -> Result<()> {
    let home = short_home()?;
    let selection =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("auth-office.json")))?;
    let nominal = DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        selection.clone(),
        AuthCredentialsStoreMode::File,
    )?;
    let ephemeral = DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        selection,
        AuthCredentialsStoreMode::Ephemeral,
    )?;
    let identities = candidates(&nominal, DaemonProfileLookup::AnyBackend)?
        .into_iter()
        .map(|candidate| candidate.auth_profile().clone())
        .collect::<Vec<_>>();
    assert_eq!(
        identities,
        vec![
            nominal.auth_profile().clone(),
            ephemeral.auth_profile().clone()
        ],
    );
    Ok(())
}
