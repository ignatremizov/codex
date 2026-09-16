use codex_app_server_protocol::ServerAuthProfile;
use codex_app_server_protocol::ServerReadResponse;
use codex_core::config::Config;

/// Capture immutable startup metadata independently of subsequent config reads.
pub(crate) fn startup_server_metadata(config: &Config) -> ServerReadResponse {
    let identity = config.auth_file_selection.profile_identity(
        config.codex_home.as_path(),
        config.cli_auth_credentials_store_mode,
    );
    ServerReadResponse {
        auth_profile: ServerAuthProfile {
            profile_opaque_id: identity.profile_opaque_id,
            display_label: identity.display_label,
        },
    }
}
