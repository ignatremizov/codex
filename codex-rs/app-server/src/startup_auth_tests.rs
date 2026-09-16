use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use pretty_assertions::assert_eq;

use crate::AppServerRuntimeOptions;
use crate::AppServerTransport;
use crate::app_server_control_socket_path;
use crate::app_server_profile_socket_path;
use crate::validate_app_server_listen_url;

#[test]
fn bare_unix_resolves_the_captured_profile_but_explicit_socket_is_preserved() -> anyhow::Result<()>
{
    let home = tempfile::tempdir()?;
    let selection =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("auth-office.json")))?;
    let profile = selection.profile_identity(home.path(), AuthCredentialsStoreMode::File);
    let profile_socket = app_server_profile_socket_path(home.path(), &profile.profile_opaque_id)?;
    let options = AppServerRuntimeOptions {
        use_auth_profile_socket: true,
        ..Default::default()
    };
    assert_eq!(validate_app_server_listen_url("unix://")?, "unix://",);
    assert_eq!(
        options.resolve_transport(AppServerTransport::Off, home.path(), &profile)?,
        AppServerTransport::UnixSocket {
            socket_path: profile_socket,
        },
    );
    // Even an explicit legacy default socket is an intentional endpoint.
    let explicit = AppServerTransport::UnixSocket {
        socket_path: app_server_control_socket_path(home.path())?,
    };
    assert_eq!(
        AppServerRuntimeOptions::default().resolve_transport(
            explicit.clone(),
            home.path(),
            &profile,
        )?,
        explicit,
    );
    Ok(())
}
