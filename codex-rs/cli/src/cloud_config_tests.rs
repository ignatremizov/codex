use super::load_config_for_selection;
use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::write_chatgpt_auth;
use codex_config::LoaderOverrides;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::ConfigOverrides;
use codex_login::AuthFileSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test]
async fn captured_selection_controls_reload_bootstrap_and_result() -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/config/bundle"))
        .and(header("authorization", "Bearer selected-token"))
        .and(header("chatgpt-account-id", "selected-workspace"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "requirements_toml": { "enterprise_managed": [] },
        })))
        .expect(1)
        .mount(&server)
        .await;
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}/backend-api\"\n",
            server.uri(),
        ),
    )?;
    write_chatgpt_auth(
        home.path(),
        ChatGptAuthFixture::new("selected-token")
            .account_id("selected-workspace")
            .chatgpt_account_id("selected-workspace")
            .chatgpt_user_id("selected-user")
            .plan_type("enterprise"),
        AuthCredentialsStoreMode::File,
    )?;
    std::fs::rename(
        home.path().join("auth.json"),
        home.path().join("selected.json"),
    )?;
    write_chatgpt_auth(
        home.path(),
        ChatGptAuthFixture::new("other-token")
            .account_id("other-workspace")
            .chatgpt_account_id("other-workspace")
            .chatgpt_user_id("other-user")
            .plan_type("enterprise"),
        AuthCredentialsStoreMode::File,
    )?;
    let captured_home = AbsolutePathBuf::from_absolute_path(home.path())?.canonicalize()?;
    let selection = AuthFileSelection::resolve(
        captured_home.as_path(),
        Some(std::ffi::OsStr::new("selected.json")),
    )?;
    let config = load_config_for_selection(
        &CliConfigOverrides::default(),
        LoaderOverrides::without_managed_config_for_tests(),
        ConfigOverrides {
            cwd: Some(home.path().to_path_buf()),
            ..Default::default()
        },
        captured_home.clone(),
        selection.clone(),
    )
    .await?;
    assert_eq!(config.codex_home, captured_home);
    assert_eq!(config.auth_file_selection, selection);
    server.verify().await;
    Ok(())
}
