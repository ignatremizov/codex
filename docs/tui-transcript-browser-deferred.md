# Deferred transcript-browser ideas

This note keeps adjacent ideas out of the initial transcript review-mode
implementation. None of these are V1 requirements.

Integration status: Review/navigation remains a proposal at this documentation stop. The current TUI already has an owned viewport, transcript search and per-entry disclosure; this note defers extensions to those capabilities, not their existence. Both owned and inline transcript displays reuse `TranscriptView`. No implementation or CI qualification is asserted here.

## Candidate follow-ups

### Per-entry disclosure

Existing owned-screen disclosure remains unchanged. A later live Review-browser extension could expose additional command or exploration detail without switching the whole browser to Full. It must reuse existing entry identity, disclosure state and height-changing layout hooks, with honest `ExecCall` output boundaries.

Only pursue this if the global Review/Full toggle proves too coarse in regular
use.

### Navigation to mutating shell commands

Commits, scripted replacements, and other state-changing commands can be useful
review landmarks. Reliable navigation needs structured execution semantics.
Command-string heuristics such as matching `git commit`, `perl -pi`, or `sed`
would be incomplete and shell-dependent.

A follow-up should first define a small canonical classification emitted by the
execution presentation layer. The proposed Review mode would keep these commands visible with capped output without jumping directly to them.

### Pointer interaction

Clickable disclosure or navigation would require scoped terminal mouse capture,
hit testing, suspend/resume restoration, and a clear contract for terminal text
selection and hyperlink activation.

Keyboard review should ship first. Pointer support should be reconsidered only
with a concrete interaction that materially improves it.

### Detailed patch browsing

Patch cells retain `FileChange` data and could expose per-file details through
the existing diff renderer. This may be useful, but patch summaries plus review
navigation solve the immediate chronology problem without materializing large
diffs.

### Search and filters

Preserve existing transcript search and its input ownership. New category filters or richer search behavior could help exceptionally long threads; they should preserve chronology and clearly indicate hidden content. Experience with Review mode and target navigation should guide such extensions.

### Preferences and configurable keys

Review mode defaults and fixed overlay-local keys should be tested before
adding config schema. A later change may persist the preferred opening mode or
add transcript-specific bindings if users need customization.

### Cache and virtualization changes

The transcript pager is already virtualized. Sparse/LRU wrapped-row caches or
incremental range replacement should be driven by profiling, not bundled with
the readability change.

### Main scrollback interaction

The TUI already has an owned fullscreen history viewport. Extending interaction with terminal-emulator scrollback in inline sessions is a separate question; do not treat the owned viewport as missing or create another viewport architecture for Review mode.

### Subagent transcript inspection

Inspecting subagent spawns, follow-ups, edits, and responses is specified
separately in `docs/tui-subagent-transcript-inspection.md`. It requires child
thread loading and nested navigation, so it is not part of transcript Review
mode.
