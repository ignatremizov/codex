//! Reapply bounded role settings without replacing the caller's live runtime authority.

use crate::agent::role::apply_role_to_config;
use crate::config::Config;
use crate::config::PermissionProfileSnapshot;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

pub(super) async fn apply_resumed_agent_role(
    config: &mut Config,
    role_name: &str,
) -> CodexResult<()> {
    let runtime_approval_policy = config.permissions.approval_policy.value();
    let runtime_approvals_reviewer = config.approvals_reviewer;
    let runtime_cwd = config.cwd.clone();
    let runtime_service_tier = config.service_tier.clone();
    let runtime_permission_profile = match config.permissions.active_permission_profile() {
        Some(active_permission_profile) => {
            PermissionProfileSnapshot::active_with_profile_workspace_roots(
                config.permissions.permission_profile().clone(),
                active_permission_profile,
                config.permissions.profile_workspace_roots().to_vec(),
            )
        }
        None => PermissionProfileSnapshot::legacy(config.permissions.permission_profile().clone()),
    };

    apply_role_to_config(config, Some(role_name))
        .await
        .map_err(CodexErr::InvalidRequest)?;
    config
        .permissions
        .approval_policy
        .set(runtime_approval_policy)
        .map_err(|err| CodexErr::InvalidRequest(format!("approval_policy is invalid: {err}")))?;
    config.approvals_reviewer = runtime_approvals_reviewer;
    config.cwd = runtime_cwd;
    config.service_tier = runtime_service_tier;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(runtime_permission_profile)
        .map_err(|err| CodexErr::InvalidRequest(format!("permission_profile is invalid: {err}")))?;
    Ok(())
}
