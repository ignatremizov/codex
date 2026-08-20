use super::*;
use crate::config::PermissionProfileSnapshot;
use crate::session::turn_context::TurnEnvironment;
use codex_exec_server::Environment;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
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

#[tokio::test]
async fn cached_remote_permissions_require_a_managed_read_only_reduction() {
    let config = crate::config::test_config().await;
    let limited = PermissionProfile::default();
    let mut limited_with_network = limited.clone();
    if let PermissionProfile::Managed { network, .. } = &mut limited_with_network {
        *network = NetworkSandboxPolicy::Enabled;
    }
    for (owner, child, accepted) in [
        (
            PermissionProfile::workspace_write(),
            PermissionProfile::read_only(),
            true,
        ),
        (PermissionProfile::workspace_write(), limited.clone(), true),
        (limited_with_network, limited.clone(), true),
        (limited, PermissionProfile::read_only(), false),
        (
            PermissionProfile::Disabled,
            PermissionProfile::read_only(),
            false,
        ),
        (
            PermissionProfile::External {
                network: NetworkSandboxPolicy::Restricted,
            },
            PermissionProfile::read_only(),
            false,
        ),
        (
            PermissionProfile::read_only(),
            PermissionProfile::workspace_write(),
            false,
        ),
        (
            PermissionProfile::read_only(),
            PermissionProfile::Disabled,
            false,
        ),
    ] {
        let mut parent = captured_environment(&config, Some("ws://127.0.0.1:8765".to_string()));
        let TurnEnvironmentState::Ready(environment) = &mut parent.environments[0] else {
            panic!("ready environment");
        };
        // A foreign executor path must never need materialization on the app-server host.
        environment.selection.cwd = PathUri::parse("file:///C:/remote/work").expect("remote cwd");
        environment.selection.workspace_roots = vec![environment.selection.cwd.clone()];
        let EnvironmentConfigState::Ready(owner_config) = &mut environment.selection.config else {
            panic!("ready owner config");
        };
        owner_config.permission_profile = PermissionProfileSnapshot::legacy(owner.clone());
        let mut selections = parent.to_selections();
        let EnvironmentConfigState::Ready(child_config) = &mut selections[0].config else {
            panic!("ready child config");
        };
        child_config.permission_profile = PermissionProfileSnapshot::legacy(child.clone());
        let original = selections.clone();
        let result = bound_cached_environment_selections(
            &mut selections,
            &parent,
            codex_protocol::ThreadId::new(),
        );
        assert_eq!(result.is_ok(), accepted, "owner={owner:?}, child={child:?}");
        assert_eq!(selections, original);
    }
}

#[tokio::test]
async fn cached_remote_reduction_preserves_attachment_and_non_permission_checks() {
    let config = crate::config::test_config().await;
    let parent = captured_environment(&config, Some("ws://127.0.0.1:8765".to_string()));
    let original = parent.to_selections();
    let mut changed_identity = original.clone();
    changed_identity[0].environment_id = "other-executor".to_string();
    let mut changed_cwd = original.clone();
    changed_cwd[0].cwd = PathUri::parse("file:///C:/different/work").expect("remote cwd");
    let mut changed_roots = original.clone();
    changed_roots[0]
        .workspace_roots
        .push(PathUri::parse("file:///C:/extra/root").expect("extra root"));
    let mut changed_config = original.clone();
    let EnvironmentConfigState::Ready(config) = &mut changed_config[0].config else {
        panic!("ready config");
    };
    config.allow_login_shell = false;
    for mut selections in [changed_identity, changed_cwd, changed_roots, changed_config] {
        assert!(
            bound_cached_environment_selections(
                &mut selections,
                &parent,
                codex_protocol::ThreadId::new(),
            )
            .is_err()
        );
    }
    let mut from_thread = original.clone();
    from_thread[0].config = EnvironmentConfigState::FromThread;
    bound_cached_environment_selections(&mut from_thread, &parent, codex_protocol::ThreadId::new())
        .expect("pin current owner");
    assert_eq!(from_thread, original);
}

#[tokio::test]
async fn cached_local_permissions_are_intersected_with_owner_authority() {
    let config = crate::config::test_config().await;
    let parent = captured_environment(&config, /*exec_server_url*/ None);
    let original = parent.to_selections();
    let mut selections = original.clone();
    let EnvironmentConfigState::Ready(child_config) = &mut selections[0].config else {
        panic!("ready child config");
    };
    child_config.permission_profile =
        PermissionProfileSnapshot::legacy(PermissionProfile::Disabled);
    bound_cached_environment_selections(&mut selections, &parent, codex_protocol::ThreadId::new())
        .expect("intersect local permissions");
    assert_eq!(selections, original);
}
