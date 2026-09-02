# Deferred transcript-browser ideas

This note keeps adjacent ideas out of the initial transcript review-mode
implementation. None of these are V1 requirements.

Integration status: Review/navigation is implemented in the shared `TranscriptView`, with remote executable qualification pending. The current TUI already has an owned viewport, transcript search and per-entry disclosure; this note defers extensions to those capabilities, not their existence.

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
execution presentation layer. Review keeps ordinary command previews visible without classifying them as mutating-command navigation targets.

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

The shared transcript view already keeps exact loaded cell identity and bounded width-specific layout caches. Any later cache expansion should be driven by profiling and must preserve its logical anchors rather than adding a second pager or off-window renderable cache.

### Main scrollback interaction

The TUI already has an owned fullscreen history viewport. Extending interaction with terminal-emulator scrollback in inline sessions is a separate question; do not treat the owned viewport as missing or create another viewport architecture for Review mode.

### Tool history after resume

Remaining Legacy raw-only image/generic-tool compatibility and regression requirements are recorded in `docs/tui-resume-tool-history.md`. Preserve the existing command/poll and structured-item projections, full code-mode/CUA cells, and exact rollback boundaries. These category-specific follow-ups are not evidence that all tool projection is missing and are not new Review filters or pager features.

### Subagent transcript inspection

Inspecting subagent spawns, follow-ups, edits, and responses is specified
separately in `docs/tui-subagent-transcript-inspection.md`. It requires child
thread loading and nested navigation, so it is not part of transcript Review
mode.
