use codex_login::AuthFileSelection;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Host-local auth selection already captured by an embedding caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerStartupAuth {
    pub codex_home: AbsolutePathBuf,
    pub auth_file_selection: AuthFileSelection,
}

/// Validate a listen URL without resolving a profile socket before config loads.
pub fn validate_app_server_listen_url(
    value: &str,
) -> Result<String, codex_app_server_transport::AppServerTransportParseError> {
    if value != "unix://" {
        crate::AppServerTransport::from_listen_url(value)?;
    }
    Ok(value.to_string())
}

impl crate::AppServerRuntimeOptions {
    pub(crate) fn resolve_transport(
        &self,
        transport: crate::AppServerTransport,
        codex_home: &std::path::Path,
        profile: &codex_login::AuthProfileIdentity,
    ) -> std::io::Result<crate::AppServerTransport> {
        if self.use_auth_profile_socket {
            Ok(crate::AppServerTransport::UnixSocket {
                socket_path: crate::app_server_profile_socket_path(
                    codex_home,
                    &profile.profile_opaque_id,
                )?,
            })
        } else {
            Ok(transport)
        }
    }
}

#[cfg(test)]
#[path = "startup_auth_tests.rs"]
mod tests;
