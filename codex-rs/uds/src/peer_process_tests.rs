use std::io;
use std::io::Read;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::io::Write;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::process::Command;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::process::Stdio;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use pretty_assertions::assert_eq;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

const CHILD_SOCKET: &str = "CODEX_UDS_PEER_PROCESS_TEST_SOCKET";

// Run as a separate test-harness process, never as production instrumentation.
#[test]
fn peer_process_child() -> io::Result<()> {
    let Some(socket) = std::env::var_os(CHILD_SOCKET) else {
        return Ok(());
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut stream = crate::UnixStream::connect(socket).await?;
            stream.write_all(b"ready").await?;
            drop(stream);
            Ok::<_, io::Error>(())
        })?;
    let mut release = [0];
    std::io::stdin().read_exact(&mut release)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
struct ChildGuard(std::process::Child);

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn monitor_survives_disconnect_and_clone_until_process_exit() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("peer.sock");
    let mut listener = crate::UnixListener::bind(&socket).await?;
    let mut child = ChildGuard(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "peer_process::tests::peer_process_child",
                "--nocapture",
            ])
            .env(CHILD_SOCKET, &socket)
            .stdin(Stdio::piped())
            .spawn()?,
    );
    let mut stream = listener.accept().await?;
    let monitor = stream.peer_process()?;
    let retained = monitor.clone();
    drop(monitor);
    let mut handshake = Vec::new();
    stream.read_to_end(&mut handshake).await?;
    assert_eq!(handshake, b"ready".to_vec());
    drop(stream);
    assert!(!retained.has_exited()?);
    child
        .0
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("child stdin unavailable"))?
        .write_all(b"x")?;
    assert!(child.0.wait()?.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 10);
    while !retained.has_exited()? {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "peer did not exit"));
        }
        std::thread::sleep(std::time::Duration::from_millis(/*millis*/ 10));
    }
    assert!(retained.clone().has_exited()?);
    Ok(())
}
