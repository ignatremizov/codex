use super::*;
use crate::config::PermissionProfileSnapshot;
use crate::session::turn_context::TurnEnvironment;
use codex_exec_server::Environment;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn captured_environment(
    config: &Config,
    exec_server_url: Option<String>,
) -> TurnEnvironmentSnapshot {
    let environment =
        Arc::new(Environment::create_for_tests(exec_server_url).expect("environment"));
    let selection = TurnEnvironmentSelection {
        environment_id: "captured-not-default".to_string(),
        cwd: PathUri::from_abs_path(&config.cwd),
        workspace_roots: config
            .workspace_roots
            .iter()
            .map(PathUri::from_abs_path)
            .collect(),
        config: EnvironmentConfigState::Ready(EnvironmentConfig {
            allow_login_shell: true,
            workspace_roots: Vec::new(),
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            windows_sandbox_type: codex_protocol::sandbox::SandboxType::None,
            use_legacy_landlock: false,
            permission_profile: PermissionProfileSnapshot::legacy(PermissionProfile::read_only()),
            shell_environment_policy: Default::default(),
            exec_policy: None,
            mcp_policy: None,
            network_policy: None,
            selected_capability_roots: Vec::new(),
        }),
    };
    TurnEnvironmentSnapshot {
        environments: vec![TurnEnvironmentState::Ready(TurnEnvironment::new(
            selection,
            EnvironmentConfigOrigin::Owner,
            environment,
            /*shell*/ None,
        ))],
    }
}

#[tokio::test]
async fn explicit_local_workspaces_keep_captured_executor_and_use_supplied_configuration() {
    let mut config = crate::config::test_config().await;
    let parent = captured_environment(&config, /*exec_server_url*/ None);
    let old_selections = parent.to_selections();
    config.cwd = config.cwd.join("requested-cwd");
    config.workspace_roots = vec![config.cwd.join("requested-root")];
    let restored = explicit_workspace_environments(&config, &parent).expect("local override");
    assert_eq!(
        restored.to_selections(),
        vec![TurnEnvironmentSelection {
            environment_id: "captured-not-default".to_string(),
            cwd: PathUri::from_abs_path(&config.cwd),
            workspace_roots: config
                .workspace_roots
                .iter()
                .map(PathUri::from_abs_path)
                .collect(),
            config: EnvironmentConfigState::FromThread,
        }]
    );
    assert!(Arc::ptr_eq(
        &restored
            .primary()
            .expect("restored environment")
            .environment,
        &parent.primary().expect("parent environment").environment,
    ));
    assert_eq!(parent.to_selections(), old_selections);
}

#[tokio::test]
async fn explicit_remote_workspaces_preserve_unchanged_selection_and_reject_host_rebasing() {
    let mut config = crate::config::test_config().await;
    let parent = captured_environment(&config, Some("ws://127.0.0.1:8765".to_string()));
    let original = parent.to_selections();
    let unchanged =
        explicit_workspace_environments(&config, &parent).expect("unchanged remote roots");
    assert_eq!(unchanged.to_selections(), original);
    assert!(Arc::ptr_eq(
        &unchanged
            .primary()
            .expect("restored environment")
            .environment,
        &parent.primary().expect("parent environment").environment,
    ));
    config.workspace_roots = vec![config.cwd.join("host-only-root")];
    assert!(explicit_workspace_environments(&config, &parent).is_err());
    assert_eq!(parent.to_selections(), original);
}
