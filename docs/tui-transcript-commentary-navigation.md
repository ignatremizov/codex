# Transcript commentary navigation (TUI)

Status: TODO

This is the commentary-only proposal recorded in the fork's 0.153-based
series, not a description of implemented navigation. It does not incorporate
later review-mode or broader navigation proposals.

The original design assumed an inline session's `TranscriptOverlay`. In the
current integration, detailed history uses the existing owned viewport for
owned-screen sessions and a separate overlay for inline sessions. `Ctrl+T` is
the default transcript shortcut, not a fixed binding. Reconcile the proposed
actions with both presentation paths and the configured keymap before
implementation.

## Problem

The transcript overlay examined for this proposal supported row scrolling,
page scrolling, and jumping to the top or bottom, but not a fast way to move
between interim assistant updates. Recheck the active transcript's navigation
capabilities before implementing the proposal.

On a long-running task, useful commentary can be separated by large command
outputs, patches, reasoning summaries, and final responses. Finding those
updates currently requires scanning or scrolling through the full transcript.
Backtrack navigation does not solve this problem because it moves between user
inputs and is coupled to editing or rolling back a turn.

## Goal

Add previous-commentary and next-commentary actions to the transcript view.
Each action should jump directly to the beginning of the corresponding
assistant message whose canonical phase is `MessagePhase::Commentary`.

This is a transcript navigation feature. It must not require the model to call a
notification tool or emit duplicate content.

## Non-goals

- Unread counts, badges, read state, or per-thread notification state.
- A separate inbox or filtered commentary view.
- A model-visible `leave_user_message` or equivalent tool.
- New prompt guidance or changes to model context.
- Protocol, rollout, thread-store, or app-server persistence changes.
- Inferring commentary from message text, visual style, position, or prefixes.
- Changing existing scrolling, transcript closing, or backtrack semantics.

## Proposed user experience

While detailed transcript history is visible:

- `[` jumps to the previous commentary message.
- `]` jumps to the next commentary message.
- The footer shows a compact hint such as `[/] commentary`.
- A jump places the first row of the target commentary near the top of the
  transcript viewport so the update can be read in context.
- Repeated presses continue in the same direction.
- Reaching the first or last commentary leaves the viewport at that target.
  Navigation does not wrap.
- If the transcript has no commentary, the actions are no-ops.

The bindings should participate in the existing TUI keymap configuration and
conflict validation. Suggested action names are:

```text
tui.keymap.pager.previous_commentary
tui.keymap.pager.next_commentary
```

The actions are transcript-specific even if their bindings live in
`PagerKeymap`; static pager overlays must ignore them.

## Commentary identity

A navigation target is a logical assistant message with:

```text
phase == Some(MessagePhase::Commentary)
```

The phase must come from the canonical `AgentMessageItem`. Messages with
`MessagePhase::FinalAnswer` or no phase are not targets. Reasoning summaries,
tool output, user messages, plans, notices, and synthetic UI cells are not
targets.

Streaming can produce several temporary or continuation cells for one
assistant message. Those cells must form one navigation target, anchored at the
first cell or at the source-backed consolidated cell after completion. A single
commentary message must never require several key presses to pass.

The original investigation found that some history-cell representations did
not retain message phase. Inspect the current canonical source metadata before
adding a classification or new propagation path. The implementation must retain
the identity needed for navigation; it must not recover phase by inspecting
rendered output.

## Navigation state

For the original inline path, commentary navigation was proposed to belong to
`TranscriptOverlay`, not global `App` backtrack state. The owned viewport does
not instantiate that overlay; its state ownership and any shared navigation
logic need a separate design check. Neither path should reuse backtrack
selection as commentary-navigation state.

The active transcript view should maintain an optional current commentary target:

- On the first previous action, select the closest commentary beginning before
  the current viewport position.
- On the first next action, select the closest commentary beginning after the
  current viewport position.
- After a target is selected, subsequent actions move relative to that target.
- Manual scrolling clears the selected target so the next jump is based on the
  new viewport position.
- Inserting, consolidating, replacing, or trimming transcript cells must keep
  targets valid or clear the selection if its target disappears.
- Terminal resize and transcript reflow must preserve the logical target even
  when its rendered row offset changes.

The existing live tail is not a target until it represents a committed
commentary message. Once committed while detailed history is visible, it should
become available without reopening the view.

## Interaction with backtrack

Commentary navigation must remain independent of transcript backtracking:

- `[` and `]` do not prime backtrack mode.
- They do not change the highlighted user message.
- They do not affect the pending rollback or branch selection.
- Existing `Esc`, Left, Right, and Enter behavior remains unchanged.
- Enter after a commentary jump must not edit or roll back anything unless
  backtrack mode was separately activated.

## Historical implementation direction to recheck

Prefer a small classification exposed by `HistoryCell`, for example a
transcript navigation kind, over downcasting every possible assistant cell in
`TranscriptOverlay`. Both streaming and source-backed assistant cells should
retain the classification needed to group a logical commentary message.

The original direction relied on `PagerView` chunk boundaries and scrolling a
chunk into view, with a proposed beginning-alignment variant for
`TranscriptOverlay`. Recheck the current pager and owned-viewport APIs before
choosing that implementation. Do not duplicate wrapped-height calculation or
maintain absolute row offsets across reflow.

Keep the generic pager unaware of message phases. It should only receive the
target chunk index selected by the transcript overlay.

## Testing for a future implementation

Add focused unit and snapshot coverage for both the owned viewport and inline
overlay:

- Previous and next jumps across mixed user, commentary, tool, and final-answer
  cells.
- First navigation from the bottom, middle, and top of the transcript.
- No wrapping at the first and last commentary.
- No-op behavior when no commentary exists.
- Exclusion of final-answer and phase-unknown assistant messages.
- One target for a commentary message split into continuation cells.
- Target preservation across width changes and wrapped-height reflow.
- Target updates when commentary is inserted or consolidated while detailed
  history is visible.
- Selection clearing when manual scrolling occurs.
- Independence from user-message backtrack highlighting and confirmation.
- Footer hints at normal and narrow terminal widths.
- Configurable bindings and keymap conflict detection.

When implementing this visible TUI change, update or add `insta` snapshots for
the navigation hints and commentary-jump viewport in both presentation paths.

## Acceptance criteria for a future implementation

- A user can open detailed history with the configured transcript shortcut
  (`Ctrl+T` by default) and reach adjacent commentary messages with one key
  press per logical message.
- Navigation uses canonical `MessagePhase::Commentary` metadata.
- It remains correct after resize, live transcript updates, consolidation, and
  replay.
- Existing pager and backtrack controls behave exactly as before.
- No unread state, model tool, additional model guidance, or model-context
  content is introduced.
