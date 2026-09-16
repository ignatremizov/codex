//! Credential-selection checks belong to the connection that will perform work.
//!
//! Socket discovery is only a hint. Local clients require a matching home and
//! profile after connecting; remote hosts never compare host-local identities.

use crate::AppServerTarget;
use crate::RemoteAppServerEndpoint;
use crate::legacy_core::config::Config;
use codex_app_server_client::AppServerClient;
use codex_app_server_protocol::ServerAuthProfile;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;

pub(crate) fn validate_remote_selection(
    endpoint: Option<&RemoteAppServerEndpoint>,
    selection: &AuthFileSelection,
) -> std::io::Result<()> {
    if matches!(endpoint, Some(RemoteAppServerEndpoint::WebSocket { .. }))
        && matches!(selection, AuthFileSelection::Selected(_))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CODEX_AUTH_FILE cannot select credentials on a remote app server; configure the auth file on the server and reconnect without the local selector",
        ));
    }
    Ok(())
}

pub(crate) async fn connect(
    target: &AppServerTarget,
    config: &Config,
) -> color_eyre::Result<AppServerClient> {
    let endpoint = match target {
        AppServerTarget::LocalDaemon { endpoint, .. } | AppServerTarget::Remote { endpoint } => {
            endpoint
        }
        AppServerTarget::Embedded => {
            color_eyre::eyre::bail!("embedded app servers do not use a remote connection")
        }
    };
    validate_remote_selection(Some(endpoint), &config.auth_file_selection)?;
    if matches!(endpoint, RemoteAppServerEndpoint::UnixSocket { .. })
        && config.cli_auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral
    {
        color_eyre::eyre::bail!(
            "ephemeral credentials are process-local; use an embedded runtime instead of a local app-server connection"
        );
    }
    let AppServerClient::Remote(mut client) = crate::app_server_connection::connect(target).await?
    else {
        color_eyre::eyre::bail!("expected an out-of-process app server")
    };
    match endpoint {
        RemoteAppServerEndpoint::UnixSocket { .. } => {
            let expected = config
                .auth_file_selection
                .profile_identity(&config.codex_home, config.cli_auth_credentials_store_mode);
            client
                .verify_local_auth_profile(
                    &config.codex_home,
                    &ServerAuthProfile {
                        profile_opaque_id: expected.profile_opaque_id,
                        display_label: expected.display_label,
                    },
                )
                .await?;
        }
        RemoteAppServerEndpoint::WebSocket { .. } => {
            // Optional metadata must not break older or foreign-host servers.
            let _ = client.read_auth_profile().await;
        }
    }
    Ok(AppServerClient::Remote(client))
}

/// `None` selects an embedded startup. Reconnect must use `connect` instead:
/// an admitted session cannot be restored by silently changing runtimes.
pub(crate) async fn connect_for_startup(
    target: &AppServerTarget,
    config: &Config,
) -> color_eyre::Result<Option<AppServerClient>> {
    match target {
        AppServerTarget::Embedded => Ok(None),
        AppServerTarget::Remote { .. } => connect(target, config).await.map(Some),
        AppServerTarget::LocalDaemon {
            allow_embedded_fallback,
            ..
        } => match connect(target, config).await {
            Ok(client) => Ok(Some(client)),
            Err(error) if *allow_embedded_fallback => {
                tracing::debug!(%error, "local app-server profile unavailable; using embedded runtime");
                Ok(None)
            }
            Err(error) => Err(error),
        },
    }
}

pub(crate) async fn maybe_probe_daemon_socket(
    codex_home: &Path,
    selection: &AuthFileSelection,
    store_mode: AuthCredentialsStoreMode,
) -> Option<AbsolutePathBuf> {
    if store_mode == AuthCredentialsStoreMode::Ephemeral {
        return None;
    }
    let socket_path = daemon_socket_path(codex_home, selection, store_mode).ok()?;
    probe_daemon_socket(socket_path).await
}

pub(crate) fn daemon_socket_path(
    codex_home: &Path,
    selection: &AuthFileSelection,
    store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<AbsolutePathBuf> {
    let profile = selection.profile_identity(codex_home, store_mode);
    codex_app_server_client::app_server_profile_socket_path(codex_home, &profile.profile_opaque_id)
}

pub(crate) fn resolve_profile_socket_alias(
    target: &mut AppServerTarget,
    remote_addr: Option<&str>,
    config: &Config,
) -> std::io::Result<()> {
    if remote_addr == Some("unix://")
        && let AppServerTarget::Remote {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        } = target
    {
        *socket_path = daemon_socket_path(
            &config.codex_home,
            &config.auth_file_selection,
            config.cli_auth_credentials_store_mode,
        )?;
    }
    Ok(())
}

pub(crate) async fn probe_daemon_socket(socket_path: AbsolutePathBuf) -> Option<AbsolutePathBuf> {
    #[cfg(windows)]
    let (validated_path, _directory) =
        codex_uds::validate_private_socket_path(socket_path.as_path()).ok()?;
    #[cfg(windows)]
    let validated_path = AbsolutePathBuf::from_absolute_path_checked(validated_path).ok()?;
    #[cfg(windows)]
    let probe_path = validated_path.as_path();
    #[cfg(not(windows))]
    let probe_path = socket_path.as_path();
    match tokio::time::timeout(
        crate::AUTO_CONNECT_DAEMON_CONNECT_TIMEOUT,
        codex_uds::UnixStream::connect(probe_path),
    )
    .await
    {
        Ok(Ok(stream)) => {
            #[cfg(windows)]
            stream.ensure_non_elevated_peer().ok()?;
            Some(socket_path)
        }
        Ok(Err(_)) | Err(_) => None,
    }
}

#[cfg(test)]
#[path = "auth_profile_connection_tests.rs"]
mod tests;
