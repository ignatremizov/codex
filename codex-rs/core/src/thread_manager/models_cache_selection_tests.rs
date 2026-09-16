use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::build_models_manager;
use codex_features::Feature;
use codex_login::AuthFileSelection;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use core_test_support::PathExt;
use core_test_support::responses::mount_models_once;
use core_test_support::responses::mount_models_once_with_etag;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::path::Path;
use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn catalog(slug: &str) -> ModelsResponse {
    let mut model = model_info_from_slug(slug);
    model.visibility = ModelVisibility::List;
    // Supplied catalog metadata is not a runtime fallback; this marker is not serialized.
    model.used_fallback_model_metadata = false;
    ModelsResponse {
        models: vec![model],
    }
}

async fn config_for_home(home: &Path, server: &MockServer) -> Config {
    let mut config = test_config().await;
    config.codex_home = home.abs();
    config.auth_file_selection = AuthFileSelection::Default;
    config.model_catalog = None;
    config.model_provider.base_url = Some(server.uri());
    config
}

#[tokio::test]
async fn selected_file_builder_never_reads_writes_or_renews_shared_disk_cache() -> anyhow::Result<()>
{
    for seed in [false, true] {
        let home = tempdir()?;
        let cache_file = home.path().join("models_cache.json");
        let server = MockServer::start().await;
        let mut config = config_for_home(home.path(), &server).await;
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let http_client_factory = crate::test_support::default_http_client_factory();
        // Prime through the current manager so the entry has the exact provider/auth identity.
        // A selected file must bypass even an otherwise valid shared cache.
        let cached_bytes = if seed {
            let cached_catalog = catalog("previously-cached-only");
            let _cached_models =
                mount_models_once_with_etag(&server, cached_catalog.clone(), "cached-etag").await;
            let default_manager = build_models_manager(&config, auth_manager.clone());
            assert_eq!(
                default_manager
                    .raw_model_catalog(RefreshStrategy::Online, http_client_factory.clone())
                    .await,
                cached_catalog
            );
            Some(std::fs::read(&cache_file)?)
        } else {
            None
        };
        let selected_catalog = catalog("selected-account-only");
        let models_mock =
            mount_models_once_with_etag(&server, selected_catalog.clone(), "selected-etag").await;
        config.auth_file_selection =
            AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
        let manager = build_models_manager(&config, auth_manager);

        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, http_client_factory.clone())
                .await,
            codex_models_manager::bundled_models_response()?
        );
        assert_eq!(
            manager
                .raw_model_catalog(
                    RefreshStrategy::OnlineIfUncached,
                    http_client_factory.clone(),
                )
                .await,
            selected_catalog
        );
        manager
            .refresh_if_new_etag("selected-etag".to_string(), http_client_factory.clone())
            .await;
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, http_client_factory)
                .await,
            selected_catalog
        );
        assert_eq!(models_mock.requests().len(), 1);
        match cached_bytes {
            Some(bytes) => assert_eq!(std::fs::read(&cache_file)?, bytes),
            None => assert!(!cache_file.exists()),
        }
    }
    Ok(())
}

#[tokio::test]
async fn default_builder_still_reuses_shared_persisted_catalog() -> anyhow::Result<()> {
    let home = tempdir()?;
    let server = MockServer::start().await;
    let expected = catalog("default-account-cached");
    let models_mock = mount_models_once_with_etag(&server, expected.clone(), "default-etag").await;
    let cache_file = home.path().join("models_cache.json");
    let config = config_for_home(home.path(), &server).await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let first_manager = build_models_manager(&config, auth_manager.clone());
    assert_eq!(
        first_manager
            .raw_model_catalog(
                RefreshStrategy::Online,
                crate::test_support::default_http_client_factory(),
            )
            .await,
        expected
    );
    let bytes = std::fs::read(&cache_file)?;
    let manager = build_models_manager(&config, auth_manager);
    assert_eq!(
        manager
            .raw_model_catalog(
                RefreshStrategy::OnlineIfUncached,
                crate::test_support::default_http_client_factory(),
            )
            .await,
        expected
    );
    assert_eq!(models_mock.requests().len(), 1);
    assert_eq!(std::fs::read(cache_file)?, bytes);
    Ok(())
}

#[tokio::test]
async fn selected_file_builder_preserves_authoritative_configured_catalog() -> anyhow::Result<()> {
    let home = tempdir()?;
    let server = MockServer::start().await;
    let expected = catalog("configured-provider-model");
    let unused_models = mount_models_once(&server, catalog("unexpected-remote-model")).await;
    let selected = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
    for selection in [AuthFileSelection::Default, selected] {
        let mut config = config_for_home(home.path(), &server).await;
        config.auth_file_selection = selection;
        config.model_catalog = Some(expected.clone());
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let manager = build_models_manager(&config, auth_manager);
        assert_eq!(
            manager
                .raw_model_catalog(
                    RefreshStrategy::Online,
                    crate::test_support::default_http_client_factory(),
                )
                .await,
            expected
        );
    }
    assert!(unused_models.requests().is_empty());
    assert!(!home.path().join("models_cache.json").exists());
    Ok(())
}

#[tokio::test]
async fn profile_selection_preserves_provider_key_precedence_and_discovery_policy()
-> anyhow::Result<()> {
    let home = tempdir()?;
    let server = MockServer::start().await;
    let expected = catalog("provider-key-only");
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer provider-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(expected.clone()))
        .expect(/*r*/ 2)
        .mount(&server)
        .await;
    let selected = AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
    for selection in [AuthFileSelection::Default, selected] {
        for enabled in [false, true] {
            let mut config = config_for_home(home.path(), &server).await;
            config.auth_file_selection = selection.clone();
            config.model_provider.experimental_bearer_token =
                Some("provider-test-key".to_string().into());
            config.model_provider.model_catalog_url =
                Some(format!("{}/models", server.uri()).into());
            config
                .features
                .set_enabled(Feature::ApiKeyModelDiscovery, enabled)?;
            let auth_manager = AuthManager::from_auth_for_testing(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            );
            let manager = build_models_manager(&config, auth_manager);
            assert_eq!(
                manager
                    .raw_model_catalog(
                        RefreshStrategy::Online,
                        crate::test_support::default_http_client_factory(),
                    )
                    .await,
                if enabled {
                    expected.clone()
                } else {
                    codex_models_manager::bundled_models_response()?
                }
            );
        }
    }
    Ok(())
}
