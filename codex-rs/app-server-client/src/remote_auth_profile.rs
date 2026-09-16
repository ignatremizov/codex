use std::io;
use std::path::Path;
use std::time::Duration;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerAuthProfile;
use codex_app_server_protocol::ServerReadParams;
use codex_app_server_protocol::ServerReadResponse;
use tokio::time::timeout;

use super::RemoteAppServerClient;

impl RemoteAppServerClient {
    /// Metadata obtained from this connection, never from a separate probe.
    pub fn auth_profile(&self) -> Option<&ServerAuthProfile> {
        self.auth_profile.as_ref()
    }

    /// Read startup metadata from this connection before exposing it to callers.
    ///
    /// Legacy servers may lack `server/read`. Remote clients can retain their
    /// existing behavior in that case; local reuse must require positive proof.
    pub async fn read_auth_profile(&mut self) -> io::Result<Option<&ServerAuthProfile>> {
        let response = timeout(
            Duration::from_secs(2),
            self.request(ClientRequest::ServerRead {
                request_id: RequestId::String("codex-auth-profile".to_string()),
                params: ServerReadParams {},
            }),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "server/read timed out"))??;
        self.auth_profile = match response {
            Ok(response) => {
                let response: ServerReadResponse = serde_json::from_value(response)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if response.auth_profile.profile_opaque_id.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "server/read returned an empty auth profile identity",
                    ));
                }
                Some(response.auth_profile)
            }
            // Older app-server request deserializers report unknown methods as
            // Invalid Request rather than Method Not Found. Neither is proof.
            Err(error) if matches!(error.code, -32601 | -32600) => None,
            Err(error) => {
                return Err(io::Error::other(format!(
                    "server/read failed: {}",
                    error.message
                )));
            }
        };
        Ok(self.auth_profile())
    }

    /// Require local-home and credential-selection proof on this connection.
    ///
    /// Only host-local endpoints may use this check. A WebSocket endpoint can
    /// point across hosts even when its URL uses a loopback address.
    pub async fn verify_local_auth_profile(
        &mut self,
        expected_home: &Path,
        expected: &ServerAuthProfile,
    ) -> io::Result<()> {
        if self.codex_home().map(Path::new) != Some(expected_home) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local app server uses a different CODEX_HOME",
            ));
        }
        let actual = self.read_auth_profile().await?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local app server cannot prove its auth profile; restart it with a current Codex",
            )
        })?;
        if actual.profile_opaque_id != expected.profile_opaque_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local app server uses a different auth profile; select its CODEX_AUTH_FILE or start the matching server explicitly",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "remote_auth_profile_tests.rs"]
mod tests;
