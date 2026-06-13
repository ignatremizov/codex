# Request user input overlay (TUI)

This note documents the TUI overlay used to gather answers for
app-server `ToolRequestUserInputParams`. The implementation lives in
`codex-rs/tui/src/bottom_pane/request_user_input/`.

## Overview

The overlay renders one question at a time and collects:

- A single selected option (when options exist).
- Freeform notes (always available).

Highlighting an option is separate from committing an answer. Option navigation
can leave a question unanswered, and Backspace/Delete can clear a selection.
Submitted answers contain committed option labels and optional `user_note: ...`
text. Empty or uncommitted questions have an empty answer list, not a literal
`skipped` value. The overlay can ask for confirmation before submitting unanswered
questions.

## Focus and input routing

The overlay tracks a small focus state:

- **Options**: Up/Down move the selection and Space selects.
- **Notes**: Text input edits notes for the currently selected option.

Tab opens notes for the selected option. Selecting an available "Other" option
also opens notes. Freeform-only questions use the notes composer directly.

## Navigation

- The configured accept/submit action (Enter by default) commits and advances,
  except when selecting "Other" opens its notes editor.
- Submitting the last question completes the answers, subject to unanswered-question
  confirmation.
- PageUp/PageDown navigate across questions (when multiple are present).
- The configured interrupt action (Esc by default) interrupts the run in option
  selection mode.
- When notes are open for an option question, Tab or Esc clears notes and returns
  to option selection.

List movement and composer submission respect the runtime keymap. Number keys
can select and commit an option directly.

## Optional requests

Non-blocking requests can resolve without answers after the TUI's fixed grace
period and countdown. User interaction snoozes this automatic resolution.
Blocking requests do not use that timer. The deprecated `autoResolutionMs` field
does not control the countdown; `isBlocking` selects the behavior.

## Layout priorities

The layout prefers to keep the question and all options visible. Notes and
footer hints collapse as space shrinks, with notes falling back to a single-line
"Notes: ..." input in tight terminals.
