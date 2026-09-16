use super::*;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::FileAuthStorage;
use crate::auth::storage::get_auth_file;
use pretty_assertions::assert_eq;
use serde_json::json;

async fn file_session(
    last_refresh: chrono::DateTime<Utc>,
) -> (
    tempfile::TempDir,
    FileAuthStorage,
    AuthDotJson,
    ChatgptAuthSession,
) {
    let home = tempfile::tempdir().expect("isolated auth home");
    let stored: AuthDotJson = serde_json::from_value(json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "e30.eyJzdWIiOiJzeW50aGV0aWMifQ.signature",
            "access_token": "synthetic-access",
            "refresh_token": "synthetic-refresh",
            "account_id": "account-a"
        },
        "last_refresh": last_refresh
    }))
    .expect("synthetic browser auth");
    let storage = FileAuthStorage::new(home.path().to_path_buf());
    storage.save(&stored).expect("save synthetic auth");
    let auth = CodexAuth::from_auth_dot_json(
        home.path(),
        &AuthFileSelection::Default,
        stored.clone(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        /*agent_identity_authapi_base_url*/ None,
        &crate::test_support::transport_default_auth_route_config(),
    )
    .await
    .expect("browser auth without OAuth requests");
    let inner = AuthManager::from_auth_for_testing_with_home(auth, home.path().to_path_buf());
    let session = ChatgptAuthSession {
        inner,
        expected_account_id: Some("account-a".into()),
    };
    (home, storage, stored, session)
}

#[tokio::test]
async fn source_account_switch_rejects_still_fresh_cached_browser_auth() {
    let (_home, storage, mut stored, session) = file_session(Utc::now()).await;
    assert_eq!(
        session
            .auth()
            .await
            .expect("initial source")
            .and_then(|auth| auth.get_account_id()),
        Some("account-a".into())
    );
    stored.tokens.as_mut().expect("tokens").account_id = Some("account-b".into());
    storage.save(&stored).expect("replace selected source");
    assert!(session.auth().await.is_err());
    // The general manager's guarded reload deliberately retains A; the narrow accessor must not
    // expose that retained cache as usable recording credentials after observing B on disk.
    assert_eq!(
        session
            .inner
            .auth_cached()
            .and_then(|auth| auth.get_account_id()),
        Some("account-a".into())
    );
}

#[tokio::test]
async fn source_logout_and_read_failure_reject_cached_browser_auth() {
    let (home, storage, stored, session) = file_session(Utc::now()).await;
    storage.delete().expect("log out isolated source");
    assert!(session.auth().await.is_err());
    storage.save(&stored).expect("restore isolated source");
    assert!(session.auth().await.expect("restored source").is_some());
    std::fs::write(get_auth_file(home.path()), b"invalid auth document")
        .expect("corrupt isolated source");
    assert!(session.auth().await.is_err());
}

#[tokio::test]
async fn same_account_token_rotation_reloads_before_use() {
    let (_home, storage, mut stored, session) = file_session(Utc::now()).await;
    stored.tokens.as_mut().expect("tokens").access_token = "rotated-access".into();
    storage.save(&stored).expect("rotate selected source");
    let auth = session
        .auth()
        .await
        .expect("checked source")
        .expect("browser auth");
    assert_eq!(
        (auth.get_account_id(), auth.get_token().expect("token")),
        (Some("account-a".into()), "rotated-access".into())
    );
}

#[tokio::test]
async fn proactive_refresh_failure_is_not_replaced_by_stale_credentials() {
    let stale = Utc::now() - chrono::Duration::days(TOKEN_REFRESH_INTERVAL + 1);
    let (_home, _storage, _stored, session) = file_session(stale).await;
    let cached = session.inner.auth_cached().expect("cached browser auth");
    assert!(AuthManager::should_refresh_proactively(&cached));
    // A previously classified failure exercises the real proactive path without any OAuth
    // endpoint/environment override or network request.
    let failure = RefreshTokenFailedError::new(
        RefreshTokenFailedReason::Other,
        "synthetic permanent refresh failure".into(),
    );
    session
        .inner
        .record_permanent_refresh_failure_if_unchanged(&cached, &failure);
    let error = session
        .auth()
        .await
        .expect_err("must not return cached auth");
    assert_eq!(error.to_string(), failure.to_string());
}
