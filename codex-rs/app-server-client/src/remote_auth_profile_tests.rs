use std::collections::VecDeque;
use std::path::Path;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerAuthProfile;
use codex_app_server_protocol::ServerReadResponse;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::super::RemoteAppServerClient;
use super::super::RemoteClientCommand;
use super::super::RemoteServerMetadata;
use crate::RequestResult;

fn client(home: &str, response: RequestResult) -> RemoteAppServerClient {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let worker_handle = tokio::spawn(async move {
        if let Some(RemoteClientCommand::Request {
            request,
            response_tx,
        }) = command_rx.recv().await
        {
            assert_eq!(request.method, "server/read");
            let _ = response_tx.send(Ok(response));
        }
    });
    RemoteAppServerClient {
        command_tx,
        event_rx,
        pending_events: VecDeque::new(),
        metadata: RemoteServerMetadata {
            codex_home: Some(home.to_string()),
            ..Default::default()
        },
        auth_profile: None,
        worker_handle,
    }
}

#[tokio::test]
async fn verifies_metadata_from_the_connected_server() -> Result<(), Box<dyn std::error::Error>> {
    let expected = ServerAuthProfile {
        profile_opaque_id: "profile-office".to_string(),
        display_label: "codex-office".to_string(),
    };
    let mut client = client(
        "/home",
        Ok(serde_json::to_value(ServerReadResponse {
            auth_profile: expected.clone(),
        })?),
    );
    client
        .verify_local_auth_profile(Path::new("/home"), &expected)
        .await?;
    assert_eq!(client.auth_profile(), Some(&expected));
    Ok(())
}

#[tokio::test]
async fn legacy_metadata_is_optional_remotely_but_not_local_proof()
-> Result<(), Box<dyn std::error::Error>> {
    for code in [-32601, -32600] {
        let missing = JSONRPCErrorError {
            code,
            message: "Unsupported request".to_string(),
            data: None,
        };
        let mut remote = client("/remote/home", Err(missing.clone()));
        assert_eq!(remote.read_auth_profile().await?, None);
        let mut local = client("/home", Err(missing));
        let error = local
            .verify_local_auth_profile(
                Path::new("/home"),
                &ServerAuthProfile {
                    profile_opaque_id: "expected".to_string(),
                    display_label: "codex".to_string(),
                },
            )
            .await
            .expect_err("legacy server cannot prove the selected profile");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
    Ok(())
}

#[tokio::test]
async fn matching_home_and_label_do_not_hide_a_different_profile()
-> Result<(), Box<dyn std::error::Error>> {
    let mut client = client(
        "/home",
        Ok(serde_json::to_value(ServerReadResponse {
            auth_profile: ServerAuthProfile {
                profile_opaque_id: "other".to_string(),
                display_label: "codex".to_string(),
            },
        })?),
    );
    let error = client
        .verify_local_auth_profile(
            Path::new("/home"),
            &ServerAuthProfile {
                profile_opaque_id: "expected".to_string(),
                display_label: "codex".to_string(),
            },
        )
        .await
        .expect_err("different profile must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    Ok(())
}

#[tokio::test]
async fn local_matching_rejects_a_different_initialized_home()
-> Result<(), Box<dyn std::error::Error>> {
    let expected = ServerAuthProfile {
        profile_opaque_id: "expected".to_string(),
        display_label: "codex".to_string(),
    };
    let mut client = client(
        "/other-home",
        Ok(serde_json::to_value(ServerReadResponse {
            auth_profile: expected.clone(),
        })?),
    );
    let error = client
        .verify_local_auth_profile(Path::new("/home"), &expected)
        .await
        .expect_err("local home must match initialize metadata");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    Ok(())
}
