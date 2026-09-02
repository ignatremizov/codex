# TUI transcript review mode and navigation

Status: implemented in the shared transcript viewport; remote executable validation pending

## Summary

Improve the existing transcript browser for reviewing long Codex sessions. The configured transcript shortcut defaults to `Ctrl+T`; it is not a fixed binding.

The owned viewport and inline `TranscriptOverlay` share `TranscriptView` review state, layout and logical anchors. Existing search and per-entry disclosure remain available in their current surfaces. The inline browser shows its mode in the title and hints; the owned browser uses its persistent transcript footer. This document describes implemented behavior, not successful CI qualification.

The existing Full presentation is an exact retained transcript. That is useful for auditing, but a
single file read can insert hundreds of source lines and make it difficult to
find:

- assistant commentary emitted during a turn;
- applied patches and their file summaries;
- commands that changed repository state;
- the final response.

The browser provides two focused behaviors:

1. Open the transcript in a concise **Review** mode that reuses the summaries
   already shown in the main TUI.
2. Let `[` and `]` jump to the previous or next review target: user input,
   assistant commentary, final assistant output, or a patch summary.

Pressing `v` switches between Review mode and the existing exact **Full** mode.
No source content is removed; Full mode remains available immediately.

This work does not introduce per-entry trees, mouse capture, transcript search,
arbitrary filters, or a new virtualization architecture.

## User experience

### Opening and closing

- The configured transcript shortcut (`Ctrl+T` by default) opens the live browser in Review mode in either presentation path.
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
- hyperlinks and existing styling are preserved;
- code-mode and CUA cells retain their already-complete ordinary display, including calls, reasoning and results. Review does not reintroduce removed summaries or truncate their content.

The live tail uses the same Review representation while Review mode is active.

This slice adds no new disclosure system. Preserve normal owned-screen per-entry disclosure and its key ownership. Users who need the exact retained transcript can switch the explicitly open live browser to Full with `v`.

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

Review targets are:

- a `UserHistoryCell`;
- a consolidated assistant message with
  `phase == Some(MessagePhase::Commentary)`;
- any other consolidated assistant message, including one whose phase is
  unknown;
- a `PatchHistoryCell`.

This covers turn boundaries, mid-turn commentary, and `apply_patch` results
without inventing heuristics for arbitrary shell commands.

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

No category-specific navigation key pairs are added.

### Backtrack safety

Both transcript presentations support selecting an earlier user prompt for edit/branch behavior.

While backtrack preview is active:

- Review-target navigation, detail switching and pager movement remain available;
- existing `Esc`, Left, Right, and Enter editing behavior has priority;
- Enter is enabled only after the selected message's actual content has been painted in the current layout. A separator, pending highlight, pre-layout state, resize or changed selection does not prove visibility.

Preserve guarded Legacy rollback, Paginated revert, canonical prompt identity, drafts, quarantine and successful canonical-reset fencing. Browser movement must never authorize an unseen or stale edit target.

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
q close    v detail    [ review prev    ] review next
```

`Home` and `End` address the absolute beginning and end of the loaded transcript. On paginated history, `Home` keeps requesting older pages until the true beginning is available; `End` cancels that continuation and returns to the latest history. The shared `TranscriptView` keeps logical entry anchors and bounded layout caches, so loading more history does not introduce a second pager state machine.

When backtrack preview is active, retain its edit hints and show only actions that are actually available. Do not hide working pager/browser controls or advertise confirmation before painted-content eligibility is established.

Respect modal, search, selection and disclosure ownership before routing browser keys. Plain `v` and plain brackets (including Windows AltGr bracket input) belong only to an explicitly open live browser; they must not intercept the composer or fixed historical preview. Within that browser, browser-local keys take priority over conflicting pager bindings. Static pagers retain configured bindings.

At narrow widths, lower-priority hints are omitted rather than wrapped. The
title still communicates Review versus Full. Hint groups are fitted atomically
in this priority order:

1. close;
2. detail toggle;
3. previous review target;
4. next review target;
5. pager scroll/page controls.

If the next whole group does not fit, it and lower-priority groups are omitted.
The renderer never relies on terminal clipping of a partial group.

## Scope

### Included

- Review and Full global transcript modes.
- Review mode as the default on open.
- One-key mode toggle.
- Previous/next navigation across user inputs, commentary, final assistant
  outputs, and patch summaries.
- Minimal assistant phase propagation required for commentary targets.
- Viewport anchoring across mode changes.
- Existing live-tail, append, trim, consolidation, and backtrack behavior.
- Snapshot and state coverage for the new behavior.

### Excluded

- New per-entry or per-command expansion behavior; retain existing disclosure.
- Mouse or pointer interaction.
- Navigation to arbitrary mutating shell commands.
- Patch-detail expansion.
- New search or category-filtering behavior; retain existing search.
- Persistent/configurable transcript preferences.
- Configurable transcript-specific keybindings.
- Child-agent transcript nesting.
- Main terminal-scrollback changes.
- New protocol, app-server, rollout, or model-visible fields.
- A new pager cache or virtualization design.

Deferred ideas are recorded in
`docs/tui-transcript-browser-deferred.md`.

## Implementation

- Put shared review/navigation state in the existing `TranscriptView` used by both the owned viewport and inline overlay. Preserve its logical cell identity, anchors, layout caches and viewport-bounded scrolling; do not transplant an old `PagerView` index or add a second viewport system.
- Distinguish an explicitly open live browser from the existing detailed-rendering boolean: Review still owns pager input even when cells use ordinary display representations. Preserve existing live-tail identity and invalidation.
- An explicit optional browser state distinguishes live Review/Full from fixed historical Full.
  The configured transcript shortcut and backtrack open the live browser; the resume-picker preview keeps
  its current Full-only title, hints, and pager handling.
- In Review, committed cells and the live tail use
  `display_hyperlink_lines(width)`. In Full they use
  `transcript_hyperlink_lines(width)`. Detail mode is part of the live-tail
  cache key.
- A mode toggle anchors an already queued review target, or otherwise the top
  visible committed cell, rebuilds, then aligns the same cell near the top. It
  need not preserve an unrelated wrapped-row offset inside the cell.
- Add a TUI-private `HistoryCell::transcript_navigation_kind()` that classifies
  `UserHistoryCell`, consolidated assistant output, and `PatchHistoryCell`.
  Preserve canonical commentary as its own kind, treat final or unknown-phase
  consolidated assistant messages as assistant output, and do not infer
  targets from rendered text or command names.
- Store target selection using existing logical cell identity and mutation hooks. Preserve or remap it through append, prepend, consolidation, replacement and reflow as appropriate; do not maintain independent absolute row offsets. Backtrack positioning and painted-content eligibility must be re-established after layout or selection changes.
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
  `App` retains Esc/Left/Right/Enter editing priority while permitting browser/detail/pager movement during preview, subject to modal ownership and actual painted-content confirmation.
- Keep transcript-specific state and tests in the transcript child module where
  practical; do not introduce protocol, config, rollout, or model-context
  changes.

## Validation

Authored regression coverage addresses the following behaviors. Local builds, tests and snapshot generation were not run for this integration; executable qualification remains with remote CI.

- distinct Review and Full rendering, title fallbacks, narrow atomic hints, and
  the unchanged historical Full preview;
- Review opening does not request full committed-cell output;
- navigation order, viewport-relative initial selection, no wrapping, manual
  scroll reset, append, consolidation, and replacement behavior;
- exact commentary phase propagation, metadata-free flush behavior, and replay
  patch reconstruction;
- Review/Full live-tail invalidation and logical-cell anchoring;
- modal/key ownership in both viewports, backtrack browser and pager movement, and confirmation only after the selected message's actual content has been painted;
- browser actions before the initial viewport render, narrow footer rendering,
  and replayed file-change start/completion pairs;
- unchanged deep-offset virtualization, hyperlinks, and wide-Unicode rendering.

Manual verification should use a long code-review thread: confirm file reads
are concise in Review, jump through user inputs, commentary, final outputs, and
patches, inspect exact output in Full, toggle back to the same logical area,
and exercise prompt backtracking.

## Acceptance criteria

- The configured transcript shortcut (`Ctrl+T` by default) opens a chronological Review browser in both viewports.
- Ordinary exploration and command previews remain concise in Review and exact in Full; already-complete code-mode/CUA ordinary displays remain complete in both.
- Ordinary command output is preview-capped in Review and exact in Full.
- `[` and `]` navigate user inputs, commentary, final assistant outputs, and
  patch summaries.
- Navigation uses concrete history-cell types and canonical assistant phase,
  never rendered-text or command-name heuristics.
- Mode changes preserve the logical transcript position.
- Bounded render windows, Full rendering, live updates, hyperlinks,
  backtrack, and static pager behavior remain intact.
- No pointer modes, per-entry trees, config schema, protocol surface, rollout
  format, or model-visible content changes.
