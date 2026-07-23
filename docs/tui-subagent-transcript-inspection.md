# Deferred TUI subagent transcript inspection

Status: follow-up concept; not part of transcript Review mode

## Problem

The parent transcript can show that Codex:

- spawned a subagent;
- sent it a follow-up;
- waited for or closed it;
- received its final response.

Those summaries are useful for chronology, but they do not expose the child
thread's complete commentary, tool calls, patches, or intermediate responses.
Reviewing delegated work therefore requires switching to the child thread
separately.

## Desired follow-up

From a parent transcript's subagent event, let the user open the corresponding
child transcript and inspect:

- the original delegated prompt;
- later parent-to-child follow-ups;
- child commentary and final response;
- commands and patches produced in the child thread;
- completion, interruption, or close state.

The child view should reuse the ordinary transcript rendering model rather than
introduce another one. It may open as `LiveReviewBrowser` only after child
loading produces structured command/patch cells and preserves assistant phase.
If it reuses the current flattened historical transcript conversion, it must
open as fixed Full instead.

## Questions to resolve before implementation

- Which canonical child thread/session identifier is available on every
  historical and live subagent cell?
- Can the child transcript be loaded through the existing thread transcript
  reader without blocking the TUI?
- Does opening a child replace the parent overlay, push a nested overlay, or
  open the existing thread switcher?
- How does Back return to the same parent transcript row?
- How are unavailable, deleted, ephemeral, or still-running child threads
  represented?
- Should child patches remain summaries, with exact command output available
  through the same Full toggle?
- How are nested grandchildren presented without creating an unbounded visual
  tree?

## Constraints

- Do not embed or clone the complete child transcript into the parent
  `HistoryCell`.
- Keep the child thread as the canonical source of its own history.
- Preserve parent/child attribution and direction for messages.
- Do not infer child identity from rendered labels or agent nicknames.
- Loading failure must leave the parent transcript and viewport intact.
- Reuse the transcript pager, Review/Full modes, and navigation behavior.

## Smallest useful stage

1. Add a canonical child-thread reference to the TUI subagent history model
   where the protocol already supplies one.
2. Load phase-aware structured child cells; otherwise stop at a fixed-Full
   proof of concept rather than presenting incomplete Review behavior.
3. Allow Enter on a selected spawn/result cell to load that child transcript.
4. Replace the overlay with the appropriate Review-capable or fixed-Full child
   transcript.
5. Let Back restore the parent overlay at the originating subagent cell.
6. Cover completed, running, unavailable, and nested-child cases.

Pointer interaction, inline expansion, multi-pane comparison, and a graphical
agent tree remain separate ideas.
