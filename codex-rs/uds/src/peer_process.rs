//! Connection-bound native process monitors; transport failures never imply death.

use std::io;
use std::sync::Arc;

/// A retained native identity for a local socket peer.
///
/// Capture before an application handshake and only trust the monitor after that
/// handshake succeeds. Clones remain usable after the socket or client is dropped.
#[derive(Clone)]
pub struct PeerProcess {
    inner: Arc<platform::Monitor>,
}

impl PeerProcess {
    /// Returns true only when the captured process has exited.
    /// Permission failures and unavailable native evidence are errors, not death.
    pub fn has_exited(&self) -> io::Result<bool> {
        self.inner.has_exited()
    }
}

#[cfg(unix)]
pub(super) fn capture(socket: std::os::fd::RawFd) -> io::Result<PeerProcess> {
    platform::capture(socket).map(|inner| PeerProcess {
        inner: Arc::new(inner),
    })
}

#[cfg(windows)]
pub(super) fn capture(socket: std::os::windows::io::RawSocket) -> io::Result<PeerProcess> {
    platform::capture(socket).map(|inner| PeerProcess {
        inner: Arc::new(inner),
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::os::fd::RawFd;

    pub(super) struct Monitor(OwnedFd);

    pub(super) fn capture(socket: RawFd) -> io::Result<Monitor> {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(credentials).cast(),
                &mut length,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if length as usize != std::mem::size_of_val(&credentials) || credentials.pid <= 0 {
            return Err(io::Error::other("socket peer process identity unavailable"));
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, credentials.pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Monitor(unsafe { OwnedFd::from_raw_fd(fd as RawFd) }))
    }

    impl Monitor {
        pub(super) fn has_exited(&self) -> io::Result<bool> {
            let mut event = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut event, 1, 0) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("invalid process monitor poll event"));
            }
            if result > 0 && event.revents & libc::POLLIN == 0 {
                return Err(io::Error::other("unexpected process monitor poll event"));
            }
            Ok(event.revents & libc::POLLIN != 0)
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::os::fd::RawFd;
    use std::sync::Mutex;

    pub(super) struct Monitor {
        queue: OwnedFd,
        exited: Mutex<bool>,
    }

    pub(super) fn capture(socket: RawFd) -> io::Result<Monitor> {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                std::ptr::addr_of_mut!(pid).cast(),
                &mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != std::mem::size_of_val(&pid) || pid <= 0 {
            return Err(io::Error::other("socket peer process identity unavailable"));
        }
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let event = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe { libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Monitor {
            queue,
            exited: Mutex::new(/*t*/ false),
        })
    }

    impl Monitor {
        pub(super) fn has_exited(&self) -> io::Result<bool> {
            // Serialize consumption so every clone observes a sticky exit event.
            let mut exited = self
                .exited
                .lock()
                .map_err(|_| io::Error::other("process monitor poisoned"))?;
            if *exited {
                return Ok(true);
            }
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let result = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Ok(false);
            }
            if event.flags & libc::EV_ERROR != 0 {
                // kevent is packed on macOS; formatting must not borrow its fields.
                let error = event.data;
                return Err(io::Error::other(format!(
                    "process monitor kevent error: {error}"
                )));
            }
            if event.filter != libc::EVFILT_PROC || event.fflags & libc::NOTE_EXIT == 0 {
                return Err(io::Error::other("unexpected process monitor event"));
            }
            *exited = true;
            Ok(true)
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::os::windows::io::RawSocket;
    use windows_sys::Win32::Foundation::WAIT_FAILED;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    pub(super) struct Monitor(OwnedHandle);

    pub(super) fn capture(socket: RawSocket) -> io::Result<Monitor> {
        let pid = crate::windows_peer::peer_pid(socket)?;
        let handle = unsafe {
            OpenProcess(PROCESS_SYNCHRONIZE, /*binherithandle*/ 0, pid)
        };
        if handle == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Monitor(unsafe {
            OwnedHandle::from_raw_handle(handle as _)
        }))
    }

    impl Monitor {
        pub(super) fn has_exited(&self) -> io::Result<bool> {
            match unsafe {
                WaitForSingleObject(self.0.as_raw_handle() as _, /*dwmilliseconds*/ 0)
            } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                WAIT_FAILED => Err(io::Error::last_os_error()),
                result => Err(io::Error::other(format!(
                    "unexpected process wait result: {result}"
                ))),
            }
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod platform {
    use std::io;
    use std::os::fd::RawFd;

    pub(super) struct Monitor;

    pub(super) fn capture(_socket: RawFd) -> io::Result<Monitor> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "peer process monitoring unavailable",
        ))
    }

    impl Monitor {
        pub(super) fn has_exited(&self) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "peer process monitoring unavailable",
            ))
        }
    }
}

#[cfg(test)]
#[path = "peer_process_tests.rs"]
mod tests;
