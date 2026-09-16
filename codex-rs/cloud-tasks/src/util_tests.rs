use super::*;
use codex_core::config::LoaderOverrides;
use codex_login::CodexAuth;
use http::header::AUTHORIZATION;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct OwnedTestDir {
    path: PathBuf,
}

impl OwnedTestDir {
    fn new(label: &str) -> io::Result<Self> {
        for _ in 0..100 {
            let nonce = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "codex-cloud-tasks-{label}-{}-{nonce}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique cloud-tasks test directory",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OwnedTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn config_reload_preserves_injected_auth_file_selection() {
    let codex_home = OwnedTestDir::new("home").expect("test auth home should be unique");
    let cwd = OwnedTestDir::new("cwd").expect("test cwd should be unique");
    let selection = AuthFileSelection::resolve(
        codex_home.path(),
        Some(OsStr::new("cloud-tasks-test-auth.json")),
    )
    .expect("test auth file selection should resolve");

    let config = config_builder_with_selection(codex_home.path().to_path_buf(), selection.clone())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("test config should load");
    let reloaded = config
        .rebuild_preserving_session_layers(&config)
        .await
        .expect("test config should reload");

    assert_eq!(reloaded.auth_file_selection, selection);
}

#[tokio::test]
async fn headers_use_the_injected_auth_manager() {
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());

    let headers = build_chatgpt_headers(Some(&auth_manager)).await;

    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer Access Token")
    );
    assert_eq!(
        headers
            .get("ChatGPT-Account-Id")
            .and_then(|value| value.to_str().ok()),
        Some("account_id")
    );
}
