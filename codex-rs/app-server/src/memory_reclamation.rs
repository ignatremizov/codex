//! Return already-freed allocator pages without unloading conversations.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc;

pub(crate) fn request() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    glibc::request();
}
