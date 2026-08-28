/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version string used by TUI display surfaces.
///
/// Snapshot tests run under both source and release package versions. Keep
/// layout-sensitive test rendering stable without changing the runtime version.
#[cfg(test)]
pub(crate) const CODEX_CLI_VERSION_FOR_DISPLAY: &str = "0.0.0";

#[cfg(not(test))]
pub(crate) const CODEX_CLI_VERSION_FOR_DISPLAY: &str = CODEX_CLI_VERSION;
