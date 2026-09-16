use std::collections::HashSet;

use anyhow::Result;
use anyhow::bail;
use codex_login::AuthCredentialsStoreMode;

use super::DaemonLaunchOptions;
use crate::Daemon;
use crate::client;
use crate::settings::DaemonSettings;

/// Non-starting lifecycle commands can find a captured backend without auth.
#[derive(Debug, Clone, Copy)]
pub enum DaemonProfileLookup {
    AnyBackend,
    ExactBackend(AuthCredentialsStoreMode),
}

/// Resolve only the captured home and file selection, without loading auth.
///
/// A default profile may have been started with cloud-required backend policy
/// which is unavailable offline. Inspect its bounded set of canonical backend
/// identities rather than guessing from today's local configuration.
pub async fn resolve_existing_launch(
    nominal: DaemonLaunchOptions,
    lookup: DaemonProfileLookup,
) -> Result<DaemonLaunchOptions> {
    let candidates = candidates(&nominal, lookup)?;
    let fallback = match lookup {
        DaemonProfileLookup::AnyBackend => nominal.clone(),
        DaemonProfileLookup::ExactBackend(mode) => DaemonLaunchOptions::new(
            nominal.codex_home.clone(),
            nominal.auth_file_selection.clone(),
            mode,
        )?,
    };
    let found = futures::future::try_join_all(candidates.into_iter().map(|candidate| async move {
        let daemon = Daemon::from_options(&candidate)?;
        // PID checks include process start-time fingerprints. They must not
        // depend on a responsive server, so offline stop can terminate it.
        let backend = crate::backend::pid_backend(daemon.backend_paths(&DaemonSettings::default()));
        let managed = backend.verified_running_pid().await?.is_some();
        let live = managed
            || client::probe_profile(&daemon.socket_path, &candidate)
                .await
                .is_ok();
        Ok::<_, anyhow::Error>(live.then_some(candidate))
    }))
    .await?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    match found.as_slice() {
        [] => Ok(fallback),
        [candidate] => Ok(candidate.clone()),
        _ => bail!(
            "multiple running app servers use this auth-file selection with different backends; specify -c cli_auth_credentials_store=\"file\", \"keyring\", \"auto\", or \"ephemeral\" to select the intended server"
        ),
    }
}

fn candidates(
    nominal: &DaemonLaunchOptions,
    lookup: DaemonProfileLookup,
) -> Result<Vec<DaemonLaunchOptions>> {
    let modes = match lookup {
        DaemonProfileLookup::AnyBackend => vec![
            AuthCredentialsStoreMode::File,
            AuthCredentialsStoreMode::Keyring,
            AuthCredentialsStoreMode::Auto,
            AuthCredentialsStoreMode::Ephemeral,
        ],
        DaemonProfileLookup::ExactBackend(mode) => vec![mode],
    };
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for mode in modes {
        let candidate = DaemonLaunchOptions::new(
            nominal.codex_home.clone(),
            nominal.auth_file_selection.clone(),
            mode,
        )?;
        if seen.insert(candidate.auth_profile().profile_opaque_id.clone()) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

#[cfg(all(test, unix))]
#[path = "existing_profile_tests.rs"]
mod tests;
