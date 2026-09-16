use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use codex_app_server_transport::app_server_profile_socket_path;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthFileSelection;
use codex_login::AuthProfileIdentity;

#[path = "existing_profile.rs"]
mod existing_profile;
pub use existing_profile::DaemonProfileLookup;
pub use existing_profile::resolve_existing_launch;

/// Captured host-local configuration for one daemon and its updater.
///
/// Construct from resolved startup configuration. Child processes receive this
/// selection explicitly, including removal of an inherited selector for the
/// default profile. No lifecycle operation re-reads the ambient selection.
#[derive(Debug, Clone)]
pub struct DaemonLaunchOptions {
    codex_home: PathBuf,
    auth_file_selection: AuthFileSelection,
    store_mode: AuthCredentialsStoreMode,
    auth_profile: AuthProfileIdentity,
}

#[cfg(test)]
#[path = "launch_options_tests.rs"]
mod tests;

impl DaemonLaunchOptions {
    pub fn new(
        codex_home: PathBuf,
        auth_file_selection: AuthFileSelection,
        store_mode: AuthCredentialsStoreMode,
    ) -> std::io::Result<Self> {
        if !codex_home.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon CODEX_HOME must be absolute",
            ));
        }
        let auth_profile = auth_file_selection.profile_identity(&codex_home, store_mode);
        Ok(Self {
            codex_home,
            auth_file_selection,
            store_mode,
            auth_profile,
        })
    }

    pub fn auth_profile(&self) -> &AuthProfileIdentity {
        &self.auth_profile
    }

    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    pub fn socket_path(&self) -> std::io::Result<PathBuf> {
        Ok(
            app_server_profile_socket_path(&self.codex_home, &self.auth_profile.profile_opaque_id)?
                .into_path_buf(),
        )
    }

    pub(crate) fn configure_command(&self, command: &mut Command) {
        command.env("CODEX_HOME", &self.codex_home);
        match &self.auth_file_selection {
            AuthFileSelection::Default => {
                command.env_remove("CODEX_AUTH_FILE");
            }
            AuthFileSelection::Selected(path) => {
                command.env("CODEX_AUTH_FILE", path.as_path());
            }
        }
        let mode = match self.store_mode {
            AuthCredentialsStoreMode::File => "file",
            AuthCredentialsStoreMode::Keyring => "keyring",
            AuthCredentialsStoreMode::Auto => "auto",
            AuthCredentialsStoreMode::Ephemeral => "ephemeral",
        };
        command.args(["-c", &format!("cli_auth_credentials_store=\"{mode}\"")]);
    }
}
