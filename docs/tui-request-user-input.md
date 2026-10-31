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

## Asynchronous questions

`request_user_input_async` uses the separate non-blocking question pane. Its accepted
tool result includes `question_refs` in question order, for example `["q1", "q2"]`.
Replies to these numbered questions reach model context as `q2: Tomorrow`, without
repeating the prompt, options, transport ID, or verbose reply envelope.
The TUI uses the same short reference in submitted answer history, queued previews,
and recalled answers, so the question text is not repeated in each reply.

Spawned subagents can use the same tool when asynchronous user input is enabled and
the model advertises support. Their live questions appear with the requesting
agent's label. The TUI binds each question to its requesting UUID and server when
it arrives. Answering from Main, a sibling, or the requester itself sends genuine
user input directly to that requester through agent control; changing the selected
conversation cannot redirect the answer. The model supplies the question, and the
client derives the reply destination from the server event.

Live spawn results register the child relationship before its first question; no
picker refresh is required. If a question arrives ahead of its spawn result, the
TUI checks the server's current parent metadata before routing it. Historical
spawn provenance alone cannot bind a question from a detached root.

Enter sends the answer immediately; while editing text, the configured queue key
queues it for the requester's next turn. The selected conversation's draft stays intact. An answer
is literal text, so command prefixes and mentions cannot run commands or add
attachments. Answer submission adds no response subscription for the source model.
Existing subscriptions retain their policies.

Pending subagent questions and answer drafts follow TUI view changes and survive
unrelated turn completion or prompts. Accepted answers and replies committed by
another client dismiss only the matching requester's question. A definite refusal
preserves the draft for an explicit retry. An uncertain outcome holds the answer
without resending; check the requesting conversation for its receipt before
submitting the answer manually again. A server or ownership change never silently
redirects or adopts the requester.

Canonical transport and replay still retain the original question and stable item
identity for clients to display and dismiss the correct question. Older desktop
reply envelopes remain readable; unknown or mixed-text replies are not guessed at
or shortened. The compact projection is applied only to the model-input copy,
including compaction input.

Numbers advance across batches, compaction, and cold resume by recovering the
high-water mark from canonical artifacts, not the current model window. A fork or
paginated history revert starts from its retained lineage. Transport identities
also retain the source call ID, so matching display numbers on different branches
do not identify the same question.

The reference allocator serializes concurrent batches. Unpublished reservations may
leave gaps, but a failed or overflowing reservation does not reset the high-water
mark. A durable session that cannot read its canonical artifacts rejects reference
allocation rather than guessing that numbering starts at one. Short references are
display/model-context labels, not proof of permission, pending state, or delivery.
