# Durable tool history in resumed TUI transcripts

Status: follow-up design; not part of transcript Review mode

## Problem

The live TUI builds command, patch, image, and other tool cells as events arrive.
Those `HistoryCell` values are presentation state and are not written directly
to the rollout.

The underlying tool data is durable. Legacy rollouts contain raw
`ResponseItem` call/output pairs and selected tool-completion events; paginated
rollouts contain completed `TurnItem` snapshots. On `codex resume`, app-server
projects that history into `Turn`/`ThreadItem` values and the TUI rebuilds cells
from those projected items.

That projection is currently incomplete:

- raw function/custom tool calls and outputs are retained but are not generally
  converted into historical `ThreadItem` values;
- legacy `ExecCommandEnd` is intentionally transient, while legacy
  `ItemCompleted(CommandExecution)` is not persisted because the raw response
  pair is treated as its durable equivalent;
- the TUI replay path can only render a tool that survives the app-server
  projection;
- successful `FileChange` replay is handled by the transcript Review-mode
  implementation, but that does not restore unrelated tool categories.

The result is that model conversation state can resume correctly while the
human-visible TUI transcript omits commands and other tool activity that is
still present in the rollout.

## Goal

Reconstruct useful, chronological tool history when resuming or forking a
thread, using the rollout as the source of truth.

The resumed representation should match the existing live TUI closely enough
for review:

- Review mode uses the same bounded command/output previews as live cells;
- Full mode exposes the retained complete output where the live cell already
  supports it;
- patch summaries remain review-navigation targets;
- replay never executes a tool or repeats a side effect.

This is a presentation-history fix. It must not alter model-visible context or
serialize TUI render cells into rollouts.

## Smallest useful stage

1. Restore completed built-in command executions from durable call/output
   pairs, including command text, working directory when present, output,
   status, and exit code when recoverable.
2. Keep the existing structured `FileChange` reconstruction as the
   `apply_patch` path; do not parse patch text when structured changes exist.
3. Restore durable `view_image` calls as image-inspection history cells.
4. Preserve already structured MCP, web-search, and image-generation items.
5. For other matched function/custom call-output pairs, expose one generic,
   bounded tool-summary cell only if it can use canonical name, arguments,
   result, and call ID without tool-name heuristics.

If generic tool rendering requires a new broad UI abstraction, stop after the
built-in command and image paths. Missing generic summaries are preferable to a
second speculative transcript framework.

## Projection contract

Perform reconstruction in the app-server history projection, not in
`ChatWidget` and not by rereading JSONL from the TUI.

- Match calls and outputs by canonical call ID and keep their original turn.
- Prefer an existing structured `ItemCompleted` or dedicated durable end event.
- Use raw response call/output pairs only as the legacy fallback.
- Deduplicate in the projection layer so live, resumed, forked, and paginated
  consumers receive one logical `ThreadItem`.
- Continue respecting exact rollback removal and turn boundaries.
- Do not infer command type or mutation semantics from rendered output.
- Do not persist an additional copy of large command output solely for TUI
  replay.

History-mode behavior:

- Paginated history should normally consume its persisted `TurnItem`
  projection.
- Legacy history may pair raw response items because those are already its
  canonical durable representation for command tools.
- Old rollouts with incomplete or unknown payloads remain readable; unsupported
  pairs are skipped rather than causing resume to fail.

## TUI replay contract

Once app-server supplies structured historical items, replay should reuse the
ordinary presentation paths with live-only effects disabled.

- Completed command items rebuild the existing `ExecCall`/command group.
- File changes rebuild one patch summary and keep their structured navigation
  kind.
- Image inspection rebuilds its existing history cell without reopening the
  image.
- Dedicated tool items reuse their existing completed presentation.
- Generic summaries, if added, have bounded Review output and no animation,
  approval, network, or process state.

Replay ordering must follow the projected turn item order. It must not depend on
which output happened to be the active cell when the original TUI exited.

## Excluded

- Persisting or replaying terminal animation frames and output deltas.
- Recreating approval prompts that are no longer pending.
- Re-executing commands, patches, MCP calls, or image operations.
- Heuristically classifying arbitrary shell commands as mutations.
- Making every third-party tool result expandable.
- Changing compaction or model-context retention.
- Embedding serialized `HistoryCell` values in rollout JSONL.

## Validation

Automated coverage should use rollout-derived history rather than manually
constructed TUI cells:

- legacy `exec_command` call/output pairs produce one completed command item;
- paginated completed command items produce the same projected shape;
- structured patch events win over raw `apply_patch` text and do not duplicate;
- `view_image` replay renders history without invoking image handling;
- exact rollback removes tool items in the rolled-back range;
- malformed, orphaned, and unknown call/output pairs do not fail resume;
- Review uses bounded previews while Full retains exact stored output;
- resume and fork produce the same tool chronology for the shared prefix.

Manual verification should resume a thread containing a long file read, an
ordinary command, an `apply_patch`, image inspection, and one dedicated external
tool call. Compare the live transcript before exit with Review and Full after
resume, allowing only intentionally omitted transient states.
