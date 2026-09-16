use super::*;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::Role;

#[tokio::test]
async fn selected_absolute_file_does_not_authorize_a_daemon_from_another_home() -> Result<()> {
    let home = tempfile::tempdir()?;
    let other_home = tempfile::tempdir()?;
    let credential_dir = tempfile::tempdir()?;
    let credential_path = credential_dir.path().join("auth-shared.json");
    let launch = crate::DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::resolve(home.path(), Some(credential_path.as_os_str()))?,
        AuthCredentialsStoreMode::File,
    )?;
    let other = crate::DaemonLaunchOptions::new(
        other_home.path().to_path_buf(),
        AuthFileSelection::resolve(other_home.path(), Some(credential_path.as_os_str()))?,
        AuthCredentialsStoreMode::File,
    )?;
    assert_eq!(launch.auth_profile(), other.auth_profile());
    let initialized = InitializeResponse {
        user_agent: "codex_app_server_daemon/0.0.0".into(),
        codex_home: other_home.path().to_path_buf().try_into()?,
        platform_family: std::env::consts::FAMILY.into(),
        platform_os: std::env::consts::OS.into(),
    };
    let (client, _server) = tokio::io::duplex(/*max_buf_size*/ 8192);
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, /*config*/ None).await;
    let error = verify_profile(&mut client, &initialized, &launch)
        .await
        .expect_err("home mismatch");
    assert!(error.to_string().contains("CODEX_HOME"));
    Ok(())
}

#[tokio::test]
async fn profile_verification_uses_correlated_success_or_error_on_the_same_connection() -> Result<()>
{
    for response in ["match", "mismatch", "unsupported"] {
        let home = tempfile::tempdir()?;
        let launch = crate::DaemonLaunchOptions::new(
            home.path().to_path_buf(),
            AuthFileSelection::Default,
            AuthCredentialsStoreMode::File,
        )?;
        let initialized = InitializeResponse {
            user_agent: "codex_app_server_daemon/0.0.0".into(),
            codex_home: home.path().to_path_buf().try_into()?,
            platform_family: std::env::consts::FAMILY.into(),
            platform_os: std::env::consts::OS.into(),
        };
        let (client, server) = tokio::io::duplex(/*max_buf_size*/ 8192);
        let mut client =
            WebSocketStream::from_raw_socket(client, Role::Client, /*config*/ None).await;
        let mut server =
            WebSocketStream::from_raw_socket(server, Role::Server, /*config*/ None).await;
        let checking = verify_profile(&mut client, &initialized, &launch);
        let serving = async {
            let JSONRPCMessage::Request(request) = read_message(&mut server).await? else {
                anyhow::bail!("expected profile request");
            };
            assert_eq!(request.method, "server/read");
            send_message(
                &mut server,
                &JSONRPCMessage::Error(JSONRPCError {
                    id: RequestId::Integer(99),
                    error: JSONRPCErrorError {
                        code: -32601,
                        message: "unrelated-request-error".into(),
                        data: None,
                    },
                }),
            )
            .await?;
            let reply = if response == "unsupported" {
                JSONRPCMessage::Error(JSONRPCError {
                    id: request.id,
                    error: JSONRPCErrorError {
                        code: -32601,
                        message: "unsupported-profile-read".into(),
                        data: None,
                    },
                })
            } else {
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: json!({"authProfile": {
                        "profileOpaqueId": if response == "match" {
                            launch.auth_profile().profile_opaque_id.clone()
                        } else {"different".into()},
                        "displayLabel": "label-is-not-identity"
                    }}),
                })
            };
            send_message(&mut server, &reply).await
        };
        let (checked, served) = tokio::join!(checking, serving);
        served?;
        match response {
            "match" => checked?,
            "mismatch" => assert!(
                checked
                    .expect_err("profile mismatch")
                    .to_string()
                    .contains("does not match")
            ),
            "unsupported" => assert!(
                checked
                    .expect_err("unsupported profile")
                    .to_string()
                    .contains("unsupported-profile-read")
            ),
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[tokio::test]
async fn unrelated_profile_responses_do_not_renew_the_verification_deadline() -> Result<()> {
    let home = tempfile::tempdir()?;
    let launch = crate::DaemonLaunchOptions::new(
        home.path().to_path_buf(),
        AuthFileSelection::Default,
        AuthCredentialsStoreMode::File,
    )?;
    let initialized = InitializeResponse {
        user_agent: "codex_app_server_daemon/0.0.0".into(),
        codex_home: home.path().to_path_buf().try_into()?,
        platform_family: std::env::consts::FAMILY.into(),
        platform_os: std::env::consts::OS.into(),
    };
    let (client, server) = tokio::io::duplex(/*max_buf_size*/ 8192);
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, /*config*/ None).await;
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, /*config*/ None).await;
    let serving = async {
        let JSONRPCMessage::Request(request) = read_message(&mut server).await? else {
            anyhow::bail!("expected profile request");
        };
        assert_eq!(request.method, "server/read");
        for _ in 0..200 {
            send_message(
                &mut server,
                &JSONRPCMessage::Response(JSONRPCResponse {
                    id: RequestId::Integer(99),
                    result: json!({"authProfile": {
                        "profileOpaqueId": launch.auth_profile().profile_opaque_id,
                        "displayLabel": "unrelated-profile-result"
                    }}),
                }),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
        Ok::<(), anyhow::Error>(())
    };
    let error = timeout(Duration::from_secs(/*secs*/ 5), async {
        tokio::select! {
            result = verify_profile(&mut client, &initialized, &launch) => result,
            result = serving => {
                result?;
                anyhow::bail!("fixture ended before profile verification");
            }
        }
    })
    .await
    .context("profile verification exceeded its whole-response deadline")?
    .expect_err("unrelated responses cannot acknowledge this request");
    assert!(
        error
            .to_string()
            .contains("timed out waiting for server/read response")
    );
    Ok(())
}
