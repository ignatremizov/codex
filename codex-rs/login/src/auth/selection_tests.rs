use super::*;
use crate::token_data::IdTokenInfo;
use pretty_assertions::assert_eq;
use serial_test::serial;
use std::ffi::OsStr;

fn config(home: &Path, filename: &str) -> std::io::Result<AuthConfig> {
    Ok(AuthConfig {
        codex_home: home.to_path_buf(),
        auth_file_selection: AuthFileSelection::resolve(home, Some(OsStr::new(filename)))?,
        auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
        keyring_backend_kind: AuthKeyringBackendKind::Direct,
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    })
}

fn credentials(account_id: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: IdTokenInfo {
                raw_jwt: "e30.e30.signature".to_string(),
                ..Default::default()
            },
            access_token: format!("access-{account_id}"),
            refresh_token: format!("refresh-{account_id}"),
            account_id: Some(account_id.to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn missing_selected_file_does_not_load_default_persistent_or_ephemeral_auth()
-> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    for mode in [
        AuthCredentialsStoreMode::File,
        AuthCredentialsStoreMode::Ephemeral,
    ] {
        save_auth(
            home.path(),
            &credentials("default"),
            mode,
            AuthKeyringBackendKind::Direct,
        )?;
    }
    let selected = config(home.path(), "auth-office.json")?;
    assert!(
        selected
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?
            .is_none()
    );
    std::fs::write(home.path().join("auth-office.json"), "invalid-json")?;
    assert!(
        selected
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn selected_manager_reload_refresh_persistence_and_logout_stay_in_selected_file()
-> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let default = credentials("default");
    save_auth(
        home.path(),
        &default,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let selected = config(home.path(), "auth-office.json")?;
    let initial = credentials("office");
    save_auth_for_selection(
        home.path(),
        &selected.auth_file_selection,
        &initial,
        selected.auth_credentials_store_mode,
        selected.keyring_backend_kind,
    )?;
    let manager = AuthManager::new_from_auth_config(
        selected.clone(),
        /*enable_codex_api_key_env*/ false,
    )
    .await;
    let Some(CodexAuth::Chatgpt(auth)) = manager.auth_cached() else {
        panic!("selected managed ChatGPT auth should load");
    };
    let refreshed = persist_tokens(
        &auth.storage,
        /*id_token*/ None,
        Some("office-refreshed-access".to_string()),
        Some("office-refreshed-refresh".to_string()),
    )?;
    assert_eq!(
        load_auth_dot_json_for_selection(
            home.path(),
            &selected.auth_file_selection,
            selected.auth_credentials_store_mode,
            selected.keyring_backend_kind,
        )?,
        Some(refreshed.clone())
    );
    manager.reload().await;
    assert_eq!(
        manager
            .auth_cached()
            .and_then(|auth| auth.get_current_auth_json()),
        Some(refreshed)
    );
    assert!(manager.logout().await?);
    assert!(manager.auth_cached().is_none());
    assert_eq!(
        load_auth_dot_json(
            home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?,
        Some(default)
    );
    Ok(())
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn selected_ephemeral_auth_keeps_precedence_without_cross_profile_bleed()
-> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let office = config(home.path(), "auth-office.json")?;
    let other = config(home.path(), "auth-other.json")?;
    for selected in [&office, &other] {
        save_auth_for_selection(
            home.path(),
            &selected.auth_file_selection,
            &credentials("persistent"),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?;
    }
    let manager =
        AuthManager::new_from_auth_config(office.clone(), /*enable_codex_api_key_env*/ false).await;
    let external = CodexAuth::from_external_chatgpt_tokens(
        "e30.e30.signature",
        "external-office",
        /*chatgpt_plan_type*/ None,
    )?;
    manager
        .commit_external_auth(external)
        .map_err(std::io::Error::other)?;
    assert_eq!(
        office
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?
            .and_then(|auth| auth.get_account_id()),
        Some("external-office".to_string())
    );
    assert_eq!(
        other
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?
            .and_then(|auth| auth.get_account_id()),
        Some("persistent".to_string())
    );
    manager.logout().await?;
    assert!(
        office
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?
            .is_none()
    );
    assert_eq!(
        other
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?
            .and_then(|auth| auth.get_account_id()),
        Some("persistent".to_string())
    );
    Ok(())
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn api_key_login_uses_the_same_selected_store_as_load_and_logout() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let selected = config(home.path(), "auth-office.json")?;
    login_with_api_key_for_selection(
        home.path(),
        &selected.auth_file_selection,
        "selected-api-key",
        selected.auth_credentials_store_mode,
        selected.keyring_backend_kind,
    )?;
    let auth = selected
        .load_auth(/*enable_codex_api_key_env*/ false)
        .await?
        .expect("selected API key should load");
    assert_eq!(
        auth.get_token().map_err(std::io::Error::other)?,
        "selected-api-key"
    );
    assert!(!home.path().join("auth.json").exists());
    assert!(logout_for_selection(
        home.path(),
        &selected.auth_file_selection,
        selected.auth_credentials_store_mode,
        selected.keyring_backend_kind,
    )?);
    assert!(
        selected
            .load_auth(/*enable_codex_api_key_env*/ false)
            .await?
            .is_none()
    );
    Ok(())
}
