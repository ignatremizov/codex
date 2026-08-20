//! Preserve captured executors and bound restored selections to their current owner's authority.

use crate::config::Config;
use crate::config::PermissionProfileSnapshot;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::TurnEnvironmentState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::intersect_effective_permission_profiles;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;

pub(super) fn explicit_workspace_environments(
    config: &Config,
    parent: &TurnEnvironmentSnapshot,
) -> CodexResult<TurnEnvironmentSnapshot> {
    let cwd = PathUri::from_abs_path(&config.cwd);
    let workspace_roots = config
        .workspace_roots
        .iter()
        .map(PathUri::from_abs_path)
        .collect::<Vec<_>>();
    let mut captured = parent.clone();
    for environment in &mut captured.environments {
        let TurnEnvironmentState::Ready(environment) = environment else {
            return Err(CodexErr::InvalidRequest(
                "cannot override restoration workspaces while an owner environment is unavailable"
                    .to_string(),
            ));
        };
        if environment.environment.is_remote() {
            if environment.selection.cwd != cwd
                || environment.selection.workspace_roots != workspace_roots
            {
                return Err(CodexErr::InvalidRequest(
                    "host workspace overrides cannot be applied to a different remote executor workspace".to_string(),
                ));
            }
            continue;
        }
        // This is an explicit current-host boundary. Retain the exact captured executor
        // handle while normal startup derives its new configuration from the supplied Config.
        environment.selection.cwd = cwd.clone();
        environment.selection.workspace_roots = workspace_roots.clone();
        environment.config_origin = EnvironmentConfigOrigin::Thread;
        environment.shell_snapshot = futures::future::ready(None).boxed().shared();
        environment.shell_snapshot_cache = Default::default();
    }
    Ok(captured)
}

/// Bound cached child selections to the current owner's executor and authority.
/// Remote profiles require a symbolic proof; local profiles use materialized intersections.
pub(super) fn bound_cached_environment_selections(
    selections: &mut [codex_protocol::protocol::TurnEnvironmentSelection],
    parent_environments: &TurnEnvironmentSnapshot,
    thread_id: codex_protocol::ThreadId,
) -> CodexResult<()> {
    for selection in selections {
        let environment_id = &selection.environment_id;
        let invalid_environment = |reason: &str| {
            CodexErr::InvalidRequest(format!(
                "cannot resume multi-agent v2 child {thread_id}: cached environment {environment_id} {reason}"
            ))
        };
        // Matching the attachment also keeps startup on the captured owner executor.
        let owner_environment = parent_environments
            .turn_environments()
            .find(|environment| {
                let parent_selection = &environment.selection;
                parent_selection.environment_id == selection.environment_id
                    && parent_selection.cwd == selection.cwd
                    && parent_selection.workspace_roots == selection.workspace_roots
            })
            .ok_or_else(|| invalid_environment("no longer matches a ready parent environment"))?;
        let owner_config = owner_environment.config();
        let child_config = match &selection.config {
            EnvironmentConfigState::FromThread => {
                // Pin current owner authority instead of re-inferring child settings.
                selection.config = EnvironmentConfigState::Ready(owner_config.clone());
                continue;
            }
            EnvironmentConfigState::Ready(config) => config,
            EnvironmentConfigState::Pending | EnvironmentConfigState::Failed(_) => {
                return Err(invalid_environment("configuration is not ready"));
            }
        };
        let mut bounded_config = child_config.clone();
        bounded_config.permission_profile = owner_config.permission_profile.clone();
        if bounded_config != *owner_config {
            return Err(invalid_environment(
                "configuration differs from the current parent",
            ));
        }
        if child_config.permission_profile == owner_config.permission_profile {
            continue;
        }
        if owner_environment.environment.is_remote() {
            let owner_permissions = owner_config.permission_profile.permission_profile();
            let child_permissions = child_config.permission_profile.permission_profile();
            let child_is_read_only =
                child_permissions.intersect_with_read_only().as_ref() == Some(child_permissions);
            let owner_read_only = owner_permissions.intersect_with_read_only();
            if matches!(owner_permissions, PermissionProfile::Managed { .. })
                && child_is_read_only
                && owner_read_only.as_ref().is_some_and(|read_only| {
                    read_only == child_permissions
                        || read_only
                            .file_system_sandbox_policy()
                            .has_full_disk_read_access()
                })
            {
                continue;
            }
            return Err(invalid_environment(
                "permissions changed on a remote executor",
            ));
        }
        let cwd = selection
            .cwd
            .to_abs_path()
            .map_err(|_| invalid_environment("working directory is not a local absolute path"))?;
        let roots = owner_environment
            .workspace_roots()
            .iter()
            .map(PathUri::to_abs_path)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid_environment("workspace roots are not local absolute paths"))?;
        let authority = owner_environment
            .permission_profile()
            .clone()
            .materialize_project_roots_with_workspace_roots(&roots);
        let requested = child_config
            .permission_profile
            .permission_profile()
            .clone()
            .materialize_project_roots_with_workspace_roots(&roots);
        let permissions = intersect_effective_permission_profiles(&authority, &requested, &cwd)
            .map_err(|err| {
                invalid_environment(&format!("permissions cannot be intersected safely: {err}"))
            })?;
        bounded_config.permission_profile = PermissionProfileSnapshot::legacy(permissions);
        selection.config = EnvironmentConfigState::Ready(bounded_config);
    }
    Ok(())
}

#[cfg(test)]
#[path = "restore_environments_tests.rs"]
mod tests;
