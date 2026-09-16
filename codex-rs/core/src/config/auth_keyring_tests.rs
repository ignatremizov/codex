use super::*;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_config::FeatureRequirementsToml;
use codex_config::RequirementSource;
use codex_config::Sourced;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::ForcedChatgptWorkspaceIds;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::FeaturesToml;
use codex_protocol::config_types::ForcedLoginMethod;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn resolve_bootstrap_auth_keyring_backend_kind_uses_secret_auth_storage_feature()
-> std::io::Result<()> {
    let config_toml = ConfigToml {
        features: Some(FeaturesToml::from(BTreeMap::from([(
            "secret_auth_storage".to_string(),
            true,
        )]))),
        ..Default::default()
    };
    assert_eq!(
        resolve_bootstrap_auth_keyring_backend_kind(&config_toml_load_result(
            config_toml,
            /*feature_requirements*/ None,
        )?)?,
        AuthKeyringBackendKind::Secrets
    );

    let config_toml = ConfigToml {
        features: Some(FeaturesToml::from(BTreeMap::from([(
            "secret_auth_storage".to_string(),
            false,
        )]))),
        ..Default::default()
    };
    assert_eq!(
        resolve_bootstrap_auth_keyring_backend_kind(&config_toml_load_result(
            config_toml.clone(),
            /*feature_requirements*/ None,
        )?)?,
        AuthKeyringBackendKind::Direct
    );

    let requirements = Sourced::new(
        FeatureRequirementsToml {
            entries: BTreeMap::from([("secret_auth_storage".to_string(), true)]),
        },
        RequirementSource::Unknown,
    );
    assert_eq!(
        resolve_bootstrap_auth_keyring_backend_kind(&config_toml_load_result(
            config_toml,
            Some(requirements),
        )?)?,
        AuthKeyringBackendKind::Secrets
    );

    Ok(())
}

#[test]
fn managed_auth_restrictions_intersect_workspaces_and_fail_closed() {
    let config = ConfigToml {
        forced_login_method: None,
        forced_chatgpt_workspace_id: Some(ForcedChatgptWorkspaceIds::Multiple(vec![
            " denied ".to_string(),
            " allowed ".to_string(),
        ])),
        ..Default::default()
    };
    let mut requirements = ConfigRequirements {
        allowed_login_methods: Some(Sourced::new(
            vec![ForcedLoginMethod::Chatgpt],
            RequirementSource::Unknown,
        )),
        allowed_chatgpt_workspaces: Some(Sourced::new(
            vec!["allowed".to_string()],
            RequirementSource::Unknown,
        )),
        ..Default::default()
    };

    let bootstrap_config = ConfigTomlLoadResult {
        config_toml: config.clone(),
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            requirements.clone(),
            ConfigRequirementsToml::default(),
        )
        .expect("requirements should stack"),
    };
    let auth_config = bootstrap_auth_config(Path::new("codex-home"), &bootstrap_config)
        .expect("policy should resolve");
    assert_eq!(auth_config.forced_login_method, None);
    assert!(auth_config.is_login_method_allowed(ForcedLoginMethod::Chatgpt));
    assert!(!auth_config.is_login_method_allowed(ForcedLoginMethod::Api));
    assert_eq!(
        auth_config.forced_chatgpt_workspace_id,
        Some(vec!["denied".to_string(), "allowed".to_string()])
    );
    assert_eq!(
        auth_config.effective_chatgpt_workspaces(),
        Some(vec!["allowed".to_string()])
    );

    requirements.allowed_chatgpt_workspaces =
        Some(Sourced::new(Vec::new(), RequirementSource::Unknown));
    let bootstrap_config = ConfigTomlLoadResult {
        config_toml: config,
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            requirements,
            ConfigRequirementsToml::default(),
        )
        .expect("requirements should stack"),
    };
    assert_eq!(
        bootstrap_auth_config(Path::new("codex-home"), &bootstrap_config)
            .expect_err("ChatGPT-only policy without an allowed workspace must fail")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn bootstrap_auth_config_applies_managed_store_and_chatgpt_base_url() {
    let configured_store = AuthCredentialsStoreMode::File;
    let configured_url = "https://user.example/backend-api/";
    let managed_store = AuthCredentialsStoreMode::Keyring;
    let managed_url = "https://managed.example/backend-api/";
    let config_toml = ConfigToml {
        cli_auth_credentials_store: Some(configured_store),
        chatgpt_base_url: Some(configured_url.to_string()),
        ..Default::default()
    };
    let requirements = ConfigRequirements {
        cli_auth_credentials_store: Some(Sourced::new(managed_store, RequirementSource::Unknown)),
        chatgpt_base_url: Some(Sourced::new(
            managed_url.to_string(),
            RequirementSource::Unknown,
        )),
        ..Default::default()
    };
    let bootstrap_config = ConfigTomlLoadResult {
        config_toml,
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            requirements,
            ConfigRequirementsToml::default(),
        )
        .expect("requirements should stack"),
    };

    let auth_config = bootstrap_auth_config(Path::new("codex-home"), &bootstrap_config)
        .expect("managed authentication settings should resolve");

    assert_eq!(auth_config.auth_credentials_store_mode, managed_store);
    assert_eq!(auth_config.chatgpt_base_url.as_deref(), Some(managed_url));
}

fn config_toml_load_result(
    config_toml: ConfigToml,
    feature_requirements: Option<Sourced<FeatureRequirementsToml>>,
) -> std::io::Result<ConfigTomlLoadResult> {
    let requirements = ConfigRequirements {
        feature_requirements,
        ..Default::default()
    };
    Ok(ConfigTomlLoadResult {
        config_toml,
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            requirements,
            ConfigRequirementsToml::default(),
        )?,
    })
}

#[tokio::test]
async fn auth_selection_survives_bootstrap_build_and_session_reload() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let selection = AuthFileSelection::resolve(
        home.path(),
        Some(std::ffi::OsStr::new("selected-auth.json")),
    )?;
    let bootstrap_config =
        config_toml_load_result(ConfigToml::default(), /*feature_requirements*/ None)?;
    let mut expected_bootstrap = bootstrap_auth_config(home.path(), &bootstrap_config)?;
    expected_bootstrap.auth_file_selection = selection.clone();
    assert_eq!(
        bootstrap_auth_config_for_selection(home.path(), &bootstrap_config, &selection)?,
        expected_bootstrap,
    );

    let builder = super::super::ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()));
    let default_config = builder.clone().build().await?;
    let config = builder
        .clone()
        .auth_file_selection(selection.clone())
        .build()
        .await?;
    let mut expected_auth = default_config.auth_config();
    expected_auth.auth_file_selection = selection.clone();
    assert_eq!(config.auth_config(), expected_auth);

    let rebuilt = super::super::Config::rebuild_with_session_layers_and_auth_file_selection(
        &config.config_layer_stack,
        config.cwd.to_path_buf(),
        &default_config.config_layer_stack,
        config.codex_home.clone(),
        /*default_zsh_path*/ None,
        config.auth_file_selection.clone(),
    )
    .await?;
    assert_eq!(rebuilt.auth_config(), config.auth_config());

    let explicit_default = builder
        .auth_file_selection(selection)
        .harness_overrides(super::super::ConfigOverrides {
            auth_file_selection: Some(AuthFileSelection::Default),
            ..Default::default()
        })
        .build()
        .await?;
    assert_eq!(explicit_default.auth_config(), default_config.auth_config());
    Ok(())
}
