use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::build_models_manager;
use chrono::Utc;
use codex_login::AuthFileSelection;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_models_manager::cache::ModelsCacheEntry;
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
use wiremock::MockServer;

fn catalog(slug: &str) -> ModelsResponse {
    let mut model = model_info_from_slug(slug);
    model.visibility = ModelVisibility::List;
    ModelsResponse {
        models: vec![model],
    }
}

async fn config_for_home(home: &Path, server: &MockServer) -> Config {
    let mut config = test_config().await;
    config.codex_home = home.abs();
    config.model_catalog = None;
    config.model_provider.base_url = Some(server.uri());
    config
}

#[tokio::test]
async fn selected_file_builder_never_reads_writes_or_renews_shared_disk_cache() -> anyhow::Result<()>
{
    let cached_catalog = catalog("other-account-only");
    let cached_bytes = serde_json::to_vec(&ModelsCacheEntry {
        fetched_at: Utc::now(),
        etag: Some("other-account-etag".to_string()),
        client_version: Some(codex_models_manager::client_version_to_whole()),
        models: cached_catalog.models,
    })?;

    for seed in [None, Some(cached_bytes.as_slice())] {
        let home = tempdir()?;
        let cache_file = home.path().join("models_cache.json");
        if let Some(bytes) = seed {
            std::fs::write(&cache_file, bytes)?;
        }
        let server = MockServer::start().await;
        let selected_catalog = catalog("selected-account-only");
        let models_mock =
            mount_models_once_with_etag(&server, selected_catalog.clone(), "selected-etag").await;
        let mut config = config_for_home(home.path(), &server).await;
        config.auth_file_selection =
            AuthFileSelection::resolve(home.path(), Some(OsStr::new("auth-office.json")))?;
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let manager = build_models_manager(&config, auth_manager);
        let http_client_factory = crate::test_support::default_http_client_factory();

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
        match seed {
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
    let bytes = serde_json::to_vec(&ModelsCacheEntry {
        fetched_at: Utc::now(),
        etag: Some("default-etag".to_string()),
        client_version: Some(codex_models_manager::client_version_to_whole()),
        models: expected.models.clone(),
    })?;
    let cache_file = home.path().join("models_cache.json");
    std::fs::write(&cache_file, &bytes)?;
    let config = config_for_home(home.path(), &server).await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
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
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        0
    );
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
