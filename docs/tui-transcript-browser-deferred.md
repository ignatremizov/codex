# Deferred transcript-browser ideas

This note keeps adjacent ideas out of the initial transcript review-mode
implementation. None of these are V1 requirements.

## Candidate follow-ups

### Per-entry disclosure

Review mode could eventually expand one command or exploration call without
switching the whole transcript to Full. This would require local entry
identity, disclosure state, height-changing pager updates, and honest
`ExecCall` output boundaries.

Only pursue this if the global Review/Full toggle proves too coarse in regular
use.

### Navigation to mutating shell commands

Commits, scripted replacements, and other state-changing commands can be useful
review landmarks. Reliable navigation needs structured execution semantics.
Command-string heuristics such as matching `git commit`, `perl -pi`, or `sed`
would be incomplete and shell-dependent.

A follow-up should first define a small canonical classification emitted by the
execution presentation layer. Review mode already keeps these commands visible
with capped output even though V1 does not jump directly to them.

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

Transcript text search or category filters could help exceptionally long
threads. They should preserve chronology and clearly indicate hidden content.
Usage of Review mode and target navigation should guide whether either is
needed.

### Preferences and configurable keys

Review mode defaults and fixed overlay-local keys should be tested before
adding config schema. A later change may persist the preferred opening mode or
add transcript-specific bindings if users need customization.

### Cache and virtualization changes

The transcript pager is already virtualized. Sparse/LRU wrapped-row caches or
incremental range replacement should be driven by profiling, not bundled with
the readability change.

### Main scrollback interaction

Making normal terminal scrollback app-interactive requires Codex to own a
fullscreen history viewport rather than relying on terminal-emulator
scrollback. That is a separate product and architecture project.

### Subagent transcript inspection

Inspecting subagent spawns, follow-ups, edits, and responses is specified
separately in `docs/tui-subagent-transcript-inspection.md`. It requires child
thread loading and nested navigation, so it is not part of transcript Review
mode.
