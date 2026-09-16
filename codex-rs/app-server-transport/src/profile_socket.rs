use std::io;
use std::path::Path;

use codex_utils_absolute_path::AbsolutePathBuf;

/// Return a host-local endpoint for an opaque credential-selection identity.
///
/// A compact 128-bit filename keeps ordinary home paths within Unix socket
/// limits. The full identity must still be verified on the actual connection.
pub fn app_server_profile_socket_path(
    codex_home: &Path,
    profile_opaque_id: &str,
) -> io::Result<AbsolutePathBuf> {
    if profile_opaque_id.len() != 64
        || !profile_opaque_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid app-server auth profile identity",
        ));
    }
    AbsolutePathBuf::from_absolute_path(
        codex_home
            .join("app-server-control")
            .join(format!("p-{}.sock", &profile_opaque_id[..32])),
    )
}

/// Serialize startup for the selected endpoint, not for all profiles in a home.
pub fn app_server_socket_startup_lock_path(
    socket_path: &AbsolutePathBuf,
) -> io::Result<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path(socket_path.as_path().with_extension("lock"))
}

#[cfg(test)]
#[path = "profile_socket_tests.rs"]
mod tests;
