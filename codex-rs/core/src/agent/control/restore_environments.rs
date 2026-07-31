//! Apply explicit host workspaces without changing a captured executor's identity.

use crate::config::Config;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::TurnEnvironmentState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
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

#[cfg(test)]
#[path = "restore_environments_tests.rs"]
mod tests;
