use codex_config::LoaderOverrides;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use codex_utils_cli::CliConfigOverrides;
use pretty_assertions::assert_eq;

use super::captured_updater_backend;
use super::local_config_builder;

#[test]
fn updater_requires_a_typed_captured_backend_and_preserves_the_last_override() -> anyhow::Result<()>
{
    let overrides = CliConfigOverrides {
        raw_overrides: vec![
            "cli_auth_credentials_store=\"auto\"".to_string(),
            "cli_auth_credentials_store=\"keyring\"".to_string(),
        ],
    }
    .parse_overrides()
    .map_err(anyhow::Error::msg)?;
    assert_eq!(
        captured_updater_backend(&overrides)?,
        AuthCredentialsStoreMode::Keyring,
    );
    let unrelated = CliConfigOverrides {
        raw_overrides: vec!["model='cli_auth_credentials_store=\"file\"'".to_string()],
    }
    .parse_overrides()
    .map_err(anyhow::Error::msg)?;
    assert!(captured_updater_backend(&unrelated).is_err());
    Ok(())
}

#[tokio::test]
async fn offline_config_ignores_broken_credentials_and_uses_normal_backend_resolution()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    std::fs::write(home.path().join("auth.json"), "not valid auth JSON")?;
    let config = local_config_builder(
        home.path().to_path_buf(),
        AuthFileSelection::Default,
        vec![(
            "cli_auth_credentials_store".to_string(),
            toml::Value::String("keyring".to_string()),
        )],
    )
    .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
    .fallback_cwd(Some(cwd.path().to_path_buf()))
    .build()
    .await?;
    let expected = if env!("CARGO_PKG_VERSION") == "0.0.0" {
        AuthCredentialsStoreMode::File
    } else {
        AuthCredentialsStoreMode::Keyring
    };
    assert_eq!(config.cli_auth_credentials_store_mode, expected);
    assert_eq!(config.auth_file_selection, AuthFileSelection::Default);
    Ok(())
}
