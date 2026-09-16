use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ServerAuthProfile;
use codex_app_server_protocol::ServerReadParams;
use codex_app_server_protocol::ServerReadResponse;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn server_read_reports_selected_startup_profile_without_auth_contents() -> Result<()> {
    let home = tempfile::tempdir()?;
    let home_path = home.path().canonicalize()?;
    let selection =
        AuthFileSelection::resolve(&home_path, Some(std::ffi::OsStr::new("auth-office.json")))?;
    let expected = selection.profile_identity(&home_path, AuthCredentialsStoreMode::File);
    let mut server = TestAppServer::builder()
        .with_codex_home(&home_path)
        .with_env_overrides(&[("CODEX_AUTH_FILE", Some("auth-office.json"))])
        .build_initialized()
        .await?;
    let response: ServerReadResponse = server
        .request(|request_id| ClientRequest::ServerRead {
            request_id,
            params: ServerReadParams {},
        })
        .await?;
    assert_eq!(
        response,
        ServerReadResponse {
            auth_profile: ServerAuthProfile {
                profile_opaque_id: expected.profile_opaque_id,
                display_label: "codex-office".to_string(),
            },
        }
    );
    Ok(())
}
