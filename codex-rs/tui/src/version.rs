/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version string used by TUI display surfaces.
///
/// Snapshot tests run under both local source builds (`0.0.0`) and release
/// package builds (for example `0.131.0`). The version is part of bordered
/// layout width calculations, so test rendering uses a stable display version
/// instead of post-render text replacement changing line widths.
#[cfg(test)]
pub(crate) const CODEX_CLI_VERSION_FOR_DISPLAY: &str = "0.0.0";

#[cfg(not(test))]
pub(crate) const CODEX_CLI_VERSION_FOR_DISPLAY: &str = CODEX_CLI_VERSION;
