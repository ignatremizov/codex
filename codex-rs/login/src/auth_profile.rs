use std::path::Path;

use sha2::Digest;
use sha2::Sha256;

use crate::AuthCredentialsStoreMode;
use crate::AuthFileSelection;

/// Non-secret identity of a runtime's credential selection, not its account.
///
/// IDs are host-local: clients must not compare credential paths across hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthProfileIdentity {
    pub profile_opaque_id: String,
    pub display_label: String,
}

impl AuthFileSelection {
    /// Describe the captured selection without reading credential contents.
    ///
    /// The default backend policy participates in identity even when its
    /// current fallback happens to use the same file as an explicit selection.
    pub fn profile_identity(
        &self,
        codex_home: &Path,
        store_mode: AuthCredentialsStoreMode,
    ) -> AuthProfileIdentity {
        let mode = match store_mode {
            AuthCredentialsStoreMode::File => "file",
            AuthCredentialsStoreMode::Keyring => "keyring",
            AuthCredentialsStoreMode::Auto => "auto",
            AuthCredentialsStoreMode::Ephemeral => "ephemeral",
        };
        let (selection, backend, path) = match self {
            Self::Default => ("default", mode, codex_home),
            Self::Selected(path) => (
                "selected",
                if store_mode == AuthCredentialsStoreMode::Ephemeral {
                    "ephemeral"
                } else {
                    "file"
                },
                path.as_path(),
            ),
        };
        let mut digest = Sha256::new();
        for component in [
            b"codex-auth-profile-v1".as_slice(),
            std::env::consts::OS.as_bytes(),
            selection.as_bytes(),
            backend.as_bytes(),
            path.as_os_str().as_encoded_bytes(),
        ] {
            digest.update((component.len() as u64).to_le_bytes());
            digest.update(component);
        }
        let display_label = match self {
            Self::Default => "codex".to_string(),
            Self::Selected(path) => {
                let stem = path.as_path().file_stem().and_then(std::ffi::OsStr::to_str);
                match stem {
                    Some("auth" | "codex") => "codex".to_string(),
                    Some(stem)
                        if stem.len() <= 64
                            && stem
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') =>
                    {
                        let suffix = stem
                            .strip_prefix("auth-")
                            .or_else(|| stem.strip_prefix("codex-"))
                            .unwrap_or(stem);
                        format!("codex-{suffix}")
                    }
                    Some(_) | None => "codex-profile".to_string(),
                }
            }
        };
        AuthProfileIdentity {
            profile_opaque_id: format!("{:x}", digest.finalize()),
            display_label,
        }
    }
}

#[cfg(test)]
#[path = "auth_profile_tests.rs"]
mod tests;
