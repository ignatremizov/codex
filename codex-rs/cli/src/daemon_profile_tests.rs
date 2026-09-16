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

#[test]
fn start_and_restart_preserve_explicit_false_and_alias_precedence() -> anyhow::Result<()> {
    use std::collections::BTreeMap;
    let overrides = CliConfigOverrides {
        raw_overrides: vec![
            "features.imagegenext=true".to_string(),
            "features.image_generation=false".to_string(),
            "features.apps.enabled=false".to_string(),
            "features.tool_registry.some_setting='retained elsewhere'".to_string(),
        ],
    };
    for action in ["start", "restart"] {
        assert_eq!(
            super::feature_overrides(&overrides, action)?,
            BTreeMap::from([
                ("apps".to_string(), false),
                ("image_generation".to_string(), false),
            ])
        );
        let invalid = CliConfigOverrides {
            raw_overrides: vec!["features.tool_registry=true".to_string()],
        };
        assert_eq!(
            super::feature_overrides(&invalid, action)
                .expect_err("non-toggle config")
                .to_string(),
            format!("daemon {action} requires a boolean feature toggle: tool_registry")
        );
        let invalid = CliConfigOverrides {
            raw_overrides: vec!["features.apps.enabled=3".to_string()],
        };
        assert_eq!(
            super::feature_overrides(&invalid, action)
                .expect_err("not a boolean")
                .to_string(),
            format!("daemon {action} requires a boolean enablement for features.apps")
        );
    }
    Ok(())
}
