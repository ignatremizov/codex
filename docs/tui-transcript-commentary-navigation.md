# TUI transcript review mode and navigation

Status: implemented; remote validation pending

## Summary

Improve the existing `Ctrl+T` transcript overlay for reviewing long Codex
sessions.

The current overlay is an exact transcript. That is useful for auditing, but a
single file read can insert hundreds of source lines and make it difficult to
find:

- assistant commentary emitted during a turn;
- applied patches and their file summaries;
- commands that changed repository state;
- the final response.

V1 makes two focused changes:

1. Open the transcript in a concise **Review** mode that reuses the summaries
   already shown in the main TUI.
2. Let `[` and `]` jump to the previous or next review target: assistant
   commentary or a patch summary.

Pressing `v` switches between Review mode and the existing exact **Full** mode.
No source content is removed; Full mode remains available immediately.

This work does not introduce per-entry trees, mouse capture, transcript search,
arbitrary filters, or a new virtualization architecture.

## User experience

### Opening and closing

- `Ctrl+T` opens the transcript overlay in Review mode.
- Existing close keys continue to close it.
- Closing preserves existing deferred-history and terminal restoration
  behavior.
- Reopening starts in Review mode. V1 does not add a preference or config key.

### Review mode

Review mode uses each committed `HistoryCell`'s existing
`display_hyperlink_lines(width)` representation.

Consequences:

- read/search/list exploration is shown as the compact `Explored` summary;
- ordinary command output uses the configured inline output preview instead of
  copying the entire retained output;
- patch cells keep their existing file/count summaries;
- commentary, final answers, user messages, plans, and notices remain in
  chronological order;
- hyperlinks and existing styling are preserved.

The live tail uses the same Review representation while Review mode is active.

Review mode does not add disclosure markers or local expansion. Users who need
exact command text and output press `v` to switch to Full mode.

### Full mode

Full mode uses the current
`HistoryCell::transcript_hyperlink_lines(width)` representation unchanged.

It remains the exact retained transcript:

- complete formatted command output;
- existing command status and duration lines;
- existing styling and terminal hyperlinks;
- existing patch summaries;
- existing deep-offset virtualization.

Pressing `v` returns to Review mode.

### Review-target navigation

`[` jumps to the previous review target and `]` jumps to the next review
target.

V1 review targets are:

- a consolidated assistant message with
  `phase == Some(MessagePhase::Commentary)`;
- a `PatchHistoryCell`.

This intentionally covers mid-turn commentary and `apply_patch` results without
inventing heuristics for arbitrary shell commands.

Navigation rules:

- Targets remain in transcript chronology.
- Navigation does not wrap at either end.
- If no matching target exists, the key is a no-op.
- With no selected target, next chooses the first target whose chunk begins at
  or below the viewport's top content row. This includes a target already
  visible at the top or lower in the viewport.
- With no selected target, previous chooses the last target whose chunk begins
  at or above the viewport's top content row. This includes a target beginning
  exactly at the top and a long target whose body crosses the top.
- Repeated jumps continue relative to the last selected target.
- A jump aligns the target near the top of the viewport when possible.
- Manual row, page, top, or bottom scrolling clears the selected target. The
  next jump is relative to the new viewport.
- Switching Review/Full mode preserves the selected logical target.
- Appending history does not move a user who has scrolled away from the bottom.

No separate commentary-only and patch-only key pairs are added in V1.

### Backtrack safety

The existing transcript overlay also supports selecting an earlier user prompt
for edit/branch behavior.

While backtrack preview is active:

- `[` and `]` do not navigate review targets;
- `v` does not change detail mode;
- pager row/page/top/bottom scrolling is disabled;
- existing `Esc`, Left, Right, and Enter behavior has priority;
- Enter can only act on the visibly highlighted backtrack selection.

This prevents browser navigation from moving an armed edit target off-screen.

### Header and hints

The title identifies the active representation:

```text
T R A N S C R I P T · R E V I E W
T R A N S C R I P T · F U L L
```

Choose the first title that fits without clipping:

```text
T R A N S C R I P T · R E V I E W
TRANSCRIPT · REVIEW
REVIEW
```

and equivalently for Full. The fixed historical preview keeps its legacy title
behavior.

The existing scroll/page hints remain. The transcript-specific hint row adds,
when width permits:

```text
v detail    [ ] review items    q close
```

When backtrack preview is active, its existing edit hints replace browser
actions that are temporarily unavailable. Pager scrolling is disabled in that
state, so its scroll/page hint row is blank rather than showing inert keys.

Transcript-local keys are resolved before pager bindings. If a user configured
`v`, `[`, or `]` as a pager scroll binding, the transcript action wins while
this overlay is open. Static pagers continue to use the configured binding.

At narrow widths, lower-priority hints are omitted rather than wrapped. The
title still communicates Review versus Full. Hint groups are fitted atomically
in this priority order:

1. close;
2. detail toggle;
3. review-target navigation;
4. pager scroll/page controls.

If the next whole group does not fit, it and lower-priority groups are omitted.
The renderer never relies on terminal clipping of a partial group.

## Scope

### Included

- Review and Full global transcript modes.
- Review mode as the default on open.
- One-key mode toggle.
- Previous/next navigation across commentary and patch summaries.
- Minimal assistant phase propagation required for commentary targets.
- Viewport anchoring across mode changes.
- Existing live-tail, append, trim, consolidation, and backtrack behavior.
- Snapshot and state coverage for the new behavior.

### Excluded

- Per-entry or per-command expansion.
- Mouse or pointer interaction.
- Navigation to arbitrary mutating shell commands.
- Patch-detail expansion.
- Transcript text search or category filtering.
- Persistent/configurable transcript preferences.
- Configurable transcript-specific keybindings.
- Child-agent transcript nesting.
- Main terminal-scrollback changes.
- New protocol, app-server, rollout, or model-visible fields.
- A new pager cache or virtualization design.

Deferred ideas are recorded in
`docs/tui-transcript-browser-deferred.md`.

## Implementation contract

- Keep the existing `PagerView` virtualization. A mode change may rebuild
  renderables and row layout; steady scrolling must remain viewport-bounded.
- Preserve `HistoryCell index == PagerView chunk index`; the optional live tail
  remains one extra final chunk.
- Add `LiveReviewBrowser` and fixed `HistoricalFullPreview` transcript flavors.
  Normal `Ctrl+T` and backtrack use the former; the resume-picker preview keeps
  its current Full-only title, hints, and pager handling.
- In Review, committed cells and the live tail use
  `display_hyperlink_lines(width)`. In Full they use
  `transcript_hyperlink_lines(width)`. Detail mode is part of the live-tail
  cache key.
- A mode toggle anchors an already queued review target, or otherwise the top
  visible committed cell, rebuilds, then aligns the same cell near the top. It
  need not preserve an unrelated wrapped-row offset inside the cell.
- Add a TUI-private `HistoryCell::transcript_navigation_kind()` returning
  `Commentary`, `Patch`, or `None`. Classify only canonical commentary phase and
  `PatchHistoryCell`; do not infer targets from rendered text or command names.
- Store the selected target as a committed-cell index. Append and mode changes
  preserve it. Consolidation remaps the index to the replacement target or
  shifts it by the removed count, and renderable rebuilds restore its pending
  alignment. Arbitrary replacement/trim clears it. Backtrack highlights take
  positioning priority and likewise restore their pending visibility after a
  rebuild.
- Manual row/page/half-page/top/bottom scrolling clears target selection.
- Carry completed `AgentMessageItem.phase` directly through
  `ConsolidateAgentMessage` into the consolidated `AgentMarkdownCell`. Generic
  flushes retain `None`; do not add pending phase state.
- During persisted turn-item replay, reconstruct a patch cell from each
  `FileChange`, including one still in progress when the snapshot was taken.
  Buffered notification replay retains its `ItemStarted`/`ItemCompleted`
  sequencing and must not reconstruct the same patch again at completion. Do
  not change the flattened resume-picker reconstruction.
- Resolve live transcript keys as close, browser actions, then pager actions.
  Before the initial viewport render, browser actions only request that frame
  and otherwise no-op rather than deriving an anchor from uninitialized layout.
  `App` owns backtrack priority and forwards only draw/resize, close, and the
  existing Esc/Left/Right/Enter actions while preview is active.
- Keep transcript-specific state and tests in the transcript child module where
  practical; do not introduce protocol, config, rollout, or model-context
  changes.

## Validation

Automated coverage should establish:

- distinct Review and Full rendering, title fallbacks, narrow atomic hints, and
  the unchanged historical Full preview;
- Review opening does not request full committed-cell output;
- navigation order, viewport-relative initial selection, no wrapping, manual
  scroll reset, append, consolidation, and replacement behavior;
- exact commentary phase propagation, metadata-free flush behavior, and replay
  patch reconstruction;
- Review/Full live-tail invalidation and logical-cell anchoring;
- modal backtrack behavior, including ignored browser and pager keys and
  confirmation only after the selected highlight has drawn;
- browser actions before the initial viewport render, narrow footer rendering,
  and replayed file-change start/completion pairs;
- unchanged deep-offset virtualization, hyperlinks, and wide-Unicode rendering.

Manual verification should use a long code-review thread: confirm file reads are
concise in Review, jump through commentary and patches, inspect exact output in
Full, toggle back to the same logical area, and exercise prompt backtracking.

## Acceptance criteria

- `Ctrl+T` opens a concise chronological Review transcript.
- Full file-read output is absent from Review and immediately available in
  Full.
- Ordinary command output is preview-capped in Review and exact in Full.
- `[` and `]` navigate commentary and patch summaries.
- Navigation uses canonical assistant phase and concrete patch cells, never
  rendered-text heuristics.
- Mode changes preserve the logical transcript position.
- Existing virtualization, Full rendering, live updates, hyperlinks,
  backtrack, and static pager behavior remain intact.
- No pointer modes, per-entry trees, config schema, protocol surface, rollout
  format, or model-visible content changes.
