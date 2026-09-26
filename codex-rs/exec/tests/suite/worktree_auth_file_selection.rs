//! Keeps the managed-worktree trust gate on the credential profile captured at startup.

use super::git;
use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_gate_uses_selected_credentials_before_allocation() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let home = test.home_path().canonicalize()?;
    let source = test.cwd_path().canonicalize()?;
    let pool = home.join("selected-profile-worktrees");
    fs::write(source.join("tracked.txt"), "source checkout")?;
    git(&source, &["init", "--quiet"])?;
    git(&source, &["add", "."])?;
    git(
        &source,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "initial"],
    )?;

    let server = responses::start_mock_server().await;
    fs::write(
        home.join("config.toml"),
        format!(
            "features.worktrees=true\ncli_auth_credentials_store=\"ephemeral\"\nchatgpt_base_url=\"{}/backend-api\"\nopenai_base_url=\"{}/v1\"\n[desktop]\ngit-worktree-root={}\n[projects.{}]\ntrust_level=\"trusted\"\n",
            server.uri(),
            server.uri(),
            serde_json::to_string(&pool)?,
            serde_json::to_string(&source)?,
        ),
    )?;
    let default_auth = serde_json::to_vec(&serde_json::json!({
        "OPENAI_API_KEY": "unused-default-profile-test-key",
    }))?;
    fs::write(home.join("auth.json"), &default_auth)?;
    let selected_auth = serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9wbGFuX3R5cGUiOiJlbnRlcnByaXNlIiwiY2hhdGdwdF9hY2NvdW50X2lkIjoid29ya3NwYWNlLTEyMyIsImNoYXRncHRfdXNlcl9pZCI6InVzZXItMTIzIn19.signature",
            "access_token": "selected-worktree-test-token",
            "refresh_token": "selected-worktree-test-refresh",
            "account_id": "workspace-123"
        }
    }))?;
    let selected_path = home.join("worktree.auth.json");
    fs::write(&selected_path, &selected_auth)?;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/backend-api/wham/config/bundle"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer selected-worktree-test-token",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "config_toml": { "enterprise_managed": [{
                    "id": "selected-source-trust", "name": "Selected profile source trust",
                    "contents": format!(
                        "[projects.{}]\ntrust_level=\"untrusted\"\n",
                        serde_json::to_string(&source)?,
                    )
                }] }
            })),
        )
        .expect(2)
        .mount(&server)
        .await;

    for selector in [Path::new("worktree.auth.json"), selected_path.as_path()] {
        let cache = home.join("cloud-config-bundle-cache.json");
        if cache.exists() {
            fs::remove_file(cache)?;
        }
        let output = test
            .cmd()
            .env("CODEX_AUTH_FILE", selector)
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_ACCESS_TOKEN")
            .args(["--json", "--worktree", "--strict-config", "prompt"])
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("explicitly untrusted"), "{stderr}");
        assert!(
            !pool.exists(),
            "selected cloud policy must reject before allocation"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("thread.started"));
        assert_eq!(fs::read(home.join("auth.json"))?, default_auth);
        assert_eq!(fs::read(&selected_path)?, selected_auth);
    }
    Ok(())
}
