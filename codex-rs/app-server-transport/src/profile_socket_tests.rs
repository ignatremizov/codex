use pretty_assertions::assert_eq;

use super::app_server_profile_socket_path;
use super::app_server_socket_startup_lock_path;

#[test]
fn profiles_have_independent_endpoints_and_startup_locks() -> Result<(), Box<dyn std::error::Error>>
{
    let home = tempfile::tempdir()?;
    let first = app_server_profile_socket_path(home.path(), &"a".repeat(64))?;
    let second = app_server_profile_socket_path(home.path(), &"b".repeat(64))?;
    assert_ne!(first, second);
    assert_ne!(
        app_server_socket_startup_lock_path(&first)?,
        app_server_socket_startup_lock_path(&second)?
    );
    assert_eq!(
        first.as_path().parent(),
        Some(home.path().join("app-server-control").as_path())
    );
    Ok(())
}

#[test]
fn invalid_opaque_identity_cannot_escape_the_control_directory()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let error = app_server_profile_socket_path(home.path(), &format!("../{}", "a".repeat(61)))
        .expect_err("opaque identity must not contain path components");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    Ok(())
}
