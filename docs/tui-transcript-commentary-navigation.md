# Transcript commentary navigation (TUI)

Status: TODO

## Problem

The `Ctrl+T` transcript overlay supports row scrolling, page scrolling, and
jumping to the top or bottom. It does not provide a fast way to move between
interim assistant updates.

On a long-running task, useful commentary can be separated by large command
outputs, patches, reasoning summaries, and final responses. Finding those
updates currently requires scanning or scrolling through the full transcript.
Backtrack navigation does not solve this problem because it moves between user
inputs and is coupled to editing or rolling back a turn.

## Goal

Add previous-commentary and next-commentary actions to the `Ctrl+T` transcript
overlay. Each action should jump directly to the beginning of the corresponding
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

## User experience

While the transcript overlay is open:

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

The current history-cell representation does not always retain message phase.
Implementation should carry a small transcript-navigation classification or
equivalent canonical metadata into committed cells. It must not recover phase
by inspecting rendered output.

## Navigation state

Commentary navigation belongs to `TranscriptOverlay`, not global `App`
backtrack state.

The overlay should maintain an optional current commentary target:

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
commentary message. Once committed while the overlay is open, it should become
available without reopening the overlay.

## Interaction with backtrack

Commentary navigation must remain independent of transcript backtracking:

- `[` and `]` do not prime backtrack mode.
- They do not change the highlighted user message.
- They do not affect the pending rollback or branch selection.
- Existing `Esc`, Left, Right, and Enter behavior remains unchanged.
- Enter after a commentary jump must not edit or roll back anything unless
  backtrack mode was separately activated.

## Implementation direction

Prefer a small classification exposed by `HistoryCell`, for example a
transcript navigation kind, over downcasting every possible assistant cell in
`TranscriptOverlay`. Both streaming and source-backed assistant cells should
retain the classification needed to group a logical commentary message.

`PagerView` already tracks chunk boundaries and can scroll a chunk into view.
Add a variant that aligns a selected chunk's beginning with the viewport where
possible, then use it from `TranscriptOverlay`. Do not duplicate wrapped-height
calculation or maintain absolute row offsets across reflow.

Keep the generic pager unaware of message phases. It should only receive the
target chunk index selected by the transcript overlay.

## Testing

Add focused unit and snapshot coverage for:

- Previous and next jumps across mixed user, commentary, tool, and final-answer
  cells.
- First navigation from the bottom, middle, and top of the transcript.
- No wrapping at the first and last commentary.
- No-op behavior when no commentary exists.
- Exclusion of final-answer and phase-unknown assistant messages.
- One target for a commentary message split into continuation cells.
- Target preservation across width changes and wrapped-height reflow.
- Target updates when commentary is inserted or consolidated while the overlay
  is open.
- Selection clearing when manual scrolling occurs.
- Independence from user-message backtrack highlighting and confirmation.
- Footer hints at normal and narrow terminal widths.
- Configurable bindings and keymap conflict detection.

Because this changes visible TUI behavior, update or add `insta` snapshots for
the transcript overlay footer and commentary-jump viewport.

## Acceptance criteria

- A user can open `Ctrl+T` and reach adjacent commentary messages with one key
  press per logical message.
- Navigation uses canonical `MessagePhase::Commentary` metadata.
- It remains correct after resize, live transcript updates, consolidation, and
  replay.
- Existing pager and backtrack controls behave exactly as before.
- No unread state, model tool, additional model guidance, or model-context
  content is introduced.
