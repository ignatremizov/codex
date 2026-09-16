use super::*;
use crate::app_server_session::AppServerSession;
use crate::legacy_core::config::ConfigBuilder;
use codex_app_server_protocol::JSONRPCMessage;
use codex_config::LoaderOverrides;
#[cfg(unix)]
use futures::FutureExt;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio_tungstenite::tungstenite::Message;

async fn serve_profile(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    home: String,
    profile: Option<ServerAuthProfile>,
) -> color_eyre::Result<Vec<String>> {
    let mut socket = tokio_tungstenite::accept_async(stream).await?;
    let mut methods = Vec::new();
    while let Some(Ok(frame)) = socket.next().await {
        let Message::Text(text) = frame else {
            continue;
        };
        let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
            continue;
        };
        methods.push(request.method.clone());
        let response = match request.method.as_str() {
            "initialize" => json!({
                "id": request.id,
                "result": {"userAgent": "profile-test/1.0", "codexHome": home},
            }),
            "server/read" => match &profile {
                Some(profile) => json!({
                    "id": request.id,
                    "result": {"authProfile": profile},
                }),
                None => json!({
                    "id": request.id,
                    "error": {"code": -32601, "message": "Method not found"},
                }),
            },
            method => panic!("profile verification must precede other requests: {method}"),
        };
        socket
            .send(Message::Text(response.to_string().into()))
            .await?;
        if request.method == "server/read" {
            break;
        }
    }
    Ok(methods)
}

#[cfg(unix)]
#[tokio::test]
async fn actual_local_connection_requires_matching_profile_before_startup_or_reconnect()
-> color_eyre::Result<()> {
    #[derive(Clone, Copy)]
    enum ConnectionKind {
        ImplicitStartup,
        ExplicitStartup,
        Reconnect,
    }
    let home = tempfile::tempdir()?;
    let selection =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("auth-office.json")))?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .auth_file_selection(selection)
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await?;
    let expected = config
        .auth_file_selection
        .profile_identity(&config.codex_home, config.cli_auth_credentials_store_mode);
    let expected = ServerAuthProfile {
        profile_opaque_id: expected.profile_opaque_id,
        display_label: "codex-office".to_string(),
    };
    let expected_home = home.path().display().to_string();
    for (profile, reported_home) in [
        (Some(expected.clone()), expected_home.clone()),
        (
            Some(ServerAuthProfile {
                profile_opaque_id: "different-profile".to_string(),
                display_label: "codex-other".to_string(),
            }),
            expected_home.clone(),
        ),
        (None, expected_home.clone()),
        (Some(expected.clone()), "/different/local/home".to_string()),
    ] {
        for kind in [
            ConnectionKind::ImplicitStartup,
            ConnectionKind::ExplicitStartup,
            ConnectionKind::Reconnect,
        ] {
            let socket_dir = tempfile::tempdir()?;
            let socket_path = AbsolutePathBuf::from_absolute_path(socket_dir.path().join("s"))?;
            let listener = tokio::net::UnixListener::bind(socket_path.as_path())?;
            let server_home = reported_home.clone();
            let reply = profile.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                serve_profile(stream, server_home, reply).await
            });
            let endpoint = RemoteAppServerEndpoint::UnixSocket { socket_path };
            let target = match kind {
                ConnectionKind::ExplicitStartup => AppServerTarget::Remote { endpoint },
                ConnectionKind::ImplicitStartup | ConnectionKind::Reconnect => {
                    AppServerTarget::LocalDaemon { endpoint }
                }
            };
            let result = match kind {
                ConnectionKind::ImplicitStartup | ConnectionKind::ExplicitStartup => {
                    connect_for_startup(&target, &config).await
                }
                ConnectionKind::Reconnect => connect(&target, &config).await.map(Some),
            };
            let matches = profile.as_ref() == Some(&expected) && reported_home == expected_home;
            if matches {
                let client = result?.expect("verified runtime");
                let session = AppServerSession::new(
                    client,
                    crate::app_server_session::ThreadParamsMode::Embedded,
                );
                assert_eq!(
                    session.auth_profile_label(&config),
                    Some("codex-office".into())
                );
            } else if matches!(kind, ConnectionKind::ImplicitStartup) {
                assert!(result?.is_none(), "implicit startup must select embedded");
            } else {
                assert!(result.is_err());
            }
            assert_eq!(
                server.await??,
                if reported_home == expected_home {
                    vec!["initialize", "server/read"]
                } else {
                    vec!["initialize"]
                },
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn selected_socket_discovery_never_uses_the_default_daemon() -> color_eyre::Result<()> {
    // macOS's default temporary directory can exhaust the Unix socket path budget.
    let home = tempfile::tempdir_in("/tmp")?;
    let selection =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("auth-office.json")))?;
    let default_socket = daemon_socket_path(
        home.path(),
        &AuthFileSelection::Default,
        AuthCredentialsStoreMode::File,
    )?;
    std::fs::create_dir_all(default_socket.parent().expect("socket directory"))?;
    let _default_listener = tokio::net::UnixListener::bind(default_socket.as_path())?;
    assert_eq!(
        maybe_probe_daemon_socket(home.path(), &selection, AuthCredentialsStoreMode::File).await,
        None,
    );
    let identity = selection.profile_identity(home.path(), AuthCredentialsStoreMode::File);
    let selected_socket = codex_app_server_client::app_server_profile_socket_path(
        home.path(),
        &identity.profile_opaque_id,
    )?;
    std::fs::create_dir_all(selected_socket.parent().expect("socket directory"))?;
    let _selected_listener = tokio::net::UnixListener::bind(selected_socket.as_path())?;
    assert_eq!(
        maybe_probe_daemon_socket(home.path(), &selection, AuthCredentialsStoreMode::File).await,
        Some(selected_socket),
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_socket_is_not_a_profile_discovery_fallback() -> color_eyre::Result<()> {
    let home = tempfile::tempdir_in("/tmp")?;
    let legacy_socket = codex_app_server_client::app_server_control_socket_path(home.path())?;
    std::fs::create_dir_all(legacy_socket.parent().expect("socket directory"))?;
    let listener = tokio::net::UnixListener::bind(legacy_socket.as_path())?;
    assert_eq!(
        maybe_probe_daemon_socket(
            home.path(),
            &AuthFileSelection::Default,
            AuthCredentialsStoreMode::File,
        )
        .await,
        None,
    );
    assert!(listener.accept().now_or_never().is_none());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn ephemeral_profiles_are_neither_probed_nor_connected_locally() -> color_eyre::Result<()> {
    let home = tempfile::tempdir_in("/tmp")?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await?;
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::Ephemeral;
    let socket_path = daemon_socket_path(
        home.path(),
        &config.auth_file_selection,
        config.cli_auth_credentials_store_mode,
    )?;
    std::fs::create_dir_all(socket_path.parent().expect("socket directory"))?;
    let listener = tokio::net::UnixListener::bind(socket_path.as_path())?;
    assert_eq!(
        maybe_probe_daemon_socket(
            home.path(),
            &config.auth_file_selection,
            config.cli_auth_credentials_store_mode,
        )
        .await,
        None,
    );
    let endpoint = RemoteAppServerEndpoint::UnixSocket { socket_path };
    assert!(
        connect_for_startup(
            &AppServerTarget::LocalDaemon {
                endpoint: endpoint.clone()
            },
            &config
        )
        .await?
        .is_none()
    );
    assert!(
        connect(&AppServerTarget::Remote { endpoint }, &config)
            .await
            .is_err()
    );
    assert!(listener.accept().now_or_never().is_none());
    Ok(())
}

#[tokio::test]
async fn remote_legacy_server_keeps_foreign_home_semantics_and_selected_auth_is_rejected()
-> color_eyre::Result<()> {
    let home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target = AppServerTarget::Remote {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: format!("ws://{}", listener.local_addr()?),
            auth_token: None,
        },
    };
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_profile(stream, r"C:\remote\codex".to_string(), None).await
    });
    let client = connect(&target, &config).await?;
    let session =
        AppServerSession::new(client, crate::app_server_session::ThreadParamsMode::Remote);
    assert_eq!(session.auth_profile_label(&config), None);
    assert_eq!(server.await??, vec!["initialize", "server/read"]);

    config.auth_file_selection =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("auth-office.json")))?;
    let error = connect(&target, &config)
        .await
        .err()
        .expect("a local auth selector cannot target a remote server");
    let error = error
        .downcast_ref::<std::io::Error>()
        .expect("selector validation returns an I/O error before connecting");
    assert_eq!(
        (error.kind(), error.to_string()),
        (
            std::io::ErrorKind::InvalidInput,
            "CODEX_AUTH_FILE cannot select credentials on a remote app server; configure the auth file on the server and reconnect without the local selector".to_string(),
        ),
    );
    Ok(())
}

#[tokio::test]
async fn bare_local_alias_uses_captured_profile_but_explicit_paths_stay_exact()
-> color_eyre::Result<()> {
    let home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await?;
    let explicit_path = config.codex_home.join("explicit.sock");
    let selected =
        AuthFileSelection::resolve(home.path(), Some(std::ffi::OsStr::new("auth-office.json")))?;
    let mut destinations = Vec::new();
    for selection in [AuthFileSelection::Default, selected] {
        for mode in [
            AuthCredentialsStoreMode::File,
            AuthCredentialsStoreMode::Keyring,
        ] {
            config.auth_file_selection = selection.clone();
            config.cli_auth_credentials_store_mode = mode;
            let mut target = AppServerTarget::Remote {
                endpoint: RemoteAppServerEndpoint::UnixSocket {
                    socket_path: explicit_path.clone(),
                },
            };
            resolve_profile_socket_alias(&mut target, Some("unix://explicit.sock"), &config)?;
            let AppServerTarget::Remote { endpoint } = &target else {
                panic!("explicit local endpoint");
            };
            assert_eq!(
                endpoint,
                &RemoteAppServerEndpoint::UnixSocket {
                    socket_path: explicit_path.clone(),
                }
            );
            resolve_profile_socket_alias(&mut target, Some("unix://"), &config)?;
            let AppServerTarget::Remote { endpoint } = target else {
                panic!("explicit alias remains an explicit target");
            };
            let expected = daemon_socket_path(home.path(), &selection, mode)?;
            assert_eq!(
                endpoint,
                RemoteAppServerEndpoint::UnixSocket {
                    socket_path: expected.clone(),
                }
            );
            destinations.push(expected);
        }
    }
    assert_ne!(
        destinations[0], destinations[1],
        "default backend policy is part of identity"
    );
    assert_eq!(
        destinations[2], destinations[3],
        "explicit files override persistent backend policy"
    );
    assert_ne!(destinations[0], destinations[2]);
    Ok(())
}
