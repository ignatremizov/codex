use super::*;
use codex_login::AuthDotJson;
use codex_login::AuthFileSelection;
use codex_login::save_auth_for_selection;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::ffi::OsStr;

#[tokio::test]
#[serial_test::serial(codex_auth_env)]
async fn recording_preflight_and_reload_stay_on_the_captured_selected_file() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let selection =
        AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-recording.json")))?;
    let mut config = crate::legacy_core::config::ConfigBuilder::default()
        .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.path().to_path_buf())
        .auth_file_selection(selection.clone())
        .harness_overrides(crate::legacy_core::config::ConfigOverrides {
            cwd: Some(home.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
    let selected: AuthDotJson = serde_json::from_value(json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "e30.eyJzdWIiOiJzeW50aGV0aWMifQ.signature",
            "access_token": "recording-access",
            "refresh_token": "recording-refresh",
            "account_id": "recording-account"
        },
        "last_refresh": chrono::Utc::now()
    }))?;
    // A valid default login and a default ephemeral overlay must not supply or veto
    // credentials for the explicitly selected recording source.
    for mode in [
        AuthCredentialsStoreMode::File,
        AuthCredentialsStoreMode::Ephemeral,
    ] {
        save_auth_for_selection(
            home.path(),
            &AuthFileSelection::Default,
            &selected,
            mode,
            config.auth_keyring_backend_kind(),
        )?;
    }
    assert!(
        validate_browser_source(&config).is_err(),
        "missing selection cannot use default auth"
    );
    save_auth_for_selection(
        home.path(),
        &selection,
        &selected,
        AuthCredentialsStoreMode::File,
        config.auth_keyring_backend_kind(),
    )?;
    validate_browser_source(&config).map_err(anyhow::Error::msg)?;
    let prepared = PreparedTranscription::prepare(&config)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(prepared.account_id, "recording-account");

    // A later configuration choice cannot retarget the already captured manager.
    config.auth_file_selection = AuthFileSelection::Default;
    let mut rotated = selected.clone();
    rotated
        .tokens
        .as_mut()
        .expect("browser tokens")
        .access_token = "rotated-recording-access".into();
    save_auth_for_selection(
        home.path(),
        &selection,
        &rotated,
        AuthCredentialsStoreMode::File,
        config.auth_keyring_backend_kind(),
    )?;
    let credentials = prepared
        .auth
        .credentials()
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        (credentials.account_id.as_str(), credentials.bearer.as_str()),
        ("recording-account", "rotated-recording-access")
    );

    config.auth_file_selection = selection.clone();
    save_auth_for_selection(
        home.path(),
        &selection,
        &selected,
        AuthCredentialsStoreMode::Ephemeral,
        config.auth_keyring_backend_kind(),
    )?;
    assert!(
        validate_browser_source(&config).is_err(),
        "selected ephemeral override is rejected"
    );
    codex_login::auth::logout_for_selection(
        home.path(),
        &selection,
        AuthCredentialsStoreMode::Ephemeral,
        config.auth_keyring_backend_kind(),
    )?;
    validate_browser_source(&config).map_err(anyhow::Error::msg)?;
    let AuthFileSelection::Selected(path) = selection else {
        unreachable!()
    };
    std::fs::write(path.as_path(), "invalid selected credential document")?;
    assert!(validate_browser_source(&config).is_err());
    assert!(prepared.auth.auth_with_http_client_factory().await.is_err());
    std::fs::remove_file(path.as_path())?;
    assert!(validate_browser_source(&config).is_err());
    assert!(prepared.auth.credentials().await.is_err());
    Ok(())
}
