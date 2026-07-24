# Durable tool history in resumed TUI transcripts

Status: remaining compatibility gaps and regression contract; executable qualification pending

## Existing reconstruction and remaining gaps

The live TUI builds command, patch, image, and other tool cells as events arrive.
Those `HistoryCell` values are presentation state and are not written directly
to the rollout.

The underlying tool data is durable. Legacy rollouts contain raw
`ResponseItem` call/output pairs and selected tool-completion events; paginated
rollouts contain completed `TurnItem` snapshots. On `codex resume`, app-server
projects that history into `Turn`/`ThreadItem` values and the TUI rebuilds cells
from those projected items.

Substantial reconstruction already exists in the integrated owners:

- `app-server-protocol/src/protocol/thread_history/non_paginated_exec.rs` pairs Legacy `exec_command` and `write_stdin` calls with their outputs. It retains canonical call/turn identity, working-directory context, process/poll association, output and recoverable status, while preferring structured command items and respecting rollback.
- That raw fallback deliberately requires a Legacy session header and supported unnamespaced or `functions` calls. Headerless/pre-header input, unsupported names or namespaces, unrecoverable working directories and orphaned outputs do not justify fabricating a command.
- `thread_history_projection.rs` converts Paginated completed `TurnItem` records directly into structured history. MCP, web-search and image-generation history also have established projection and presentation paths.
- The TUI already renders completed command, MCP, web-search, image-view, image-generation and dynamic-tool items. Persisted nonempty completed/in-progress file changes reconstruct patch cells; failed/declined outcomes remain distinct and buffered completion does not duplicate an earlier start.
- Code-mode/CUA ordinary display already retains full calls, reasoning and results. Review preserves that completeness; it is not another truncation layer.

The remaining follow-up is category-specific compatibility, not a missing general tool-history framework. In particular, the Legacy raw-call fallback above handles commands and polling, not raw-only `view_image` or arbitrary function/custom-tool pairs. `ViewImageToolCall` is transient under the current rollout policy, so older Legacy records containing only the raw call can lack a projected image-view item even though `ThreadItem::ImageView` presentation already exists.

Any additional fallback must first establish that the canonical durable records contain sufficient identity, arguments, result and turn information. This document does not claim that all possible tools are reconstructible or that these residual fallbacks have been implemented.

## Goal

Reconstruct useful, chronological tool history when resuming or forking a
thread, using the rollout as the source of truth.

The resumed representation should match the existing live TUI closely enough
for review:

- Review mode uses ordinary live-cell representations: bounded command/output previews where already supported, and complete code-mode/CUA cells;
- Full mode exposes the retained complete output where the live cell already
  supports it;
- patch summaries remain review-navigation targets;
- replay never executes a tool or repeats a side effect.

This is a compatibility and regression contract. A future presentation-history extension must not alter model-visible context or serialize TUI render cells into rollouts. Review/Full navigation is implemented separately in the shared viewport, with executable qualification still pending.

## Smallest useful stage

1. Preserve and qualify the existing Legacy command/poll reconstruction, including command text, source turn, working-directory resolution, structured-item precedence, retained output, status and recoverable exit code.
2. Keep the existing structured `FileChange` reconstruction as the
   `apply_patch` path; do not parse patch text when structured changes exist.
3. Consider a narrowly scoped fallback for raw-only Legacy `view_image` calls when canonical identity and path provenance can be proved. Reuse the existing image-view history cell without executing the tool or reopening the image.
4. Preserve already structured MCP, web-search, and image-generation items.
5. For other matched function/custom call-output pairs, expose one generic,
   bounded tool-summary cell only if it can use canonical name, arguments,
   result, and call ID without tool-name heuristics.

If generic tool rendering requires a new broad UI abstraction, stop with existing command reconstruction and any independently justified image compatibility fix. Missing generic summaries are preferable to a second speculative transcript framework. Preserve executor/environment path authority rather than interpreting foreign paths as host files.

## Projection contract

Perform reconstruction in the app-server history projection, not in
`ChatWidget` and not by rereading JSONL from the TUI.

- Match calls and outputs by canonical call ID and keep their original turn.
- Prefer an existing structured `ItemCompleted` or dedicated durable end event when that history mode actually persists it.
- Use raw response call/output pairs only as the legacy fallback.
- Deduplicate in the projection layer so live, resumed, forked, and paginated
  consumers receive one logical `ThreadItem`.
- Continue respecting exact rollback removal, original canonical coordinates and turn boundaries, including rollback of a later poll's contribution to an earlier command.
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
- Image inspection rebuilds its existing history cell without reopening the image; a raw-only compatibility fallback must supply the structured item first.
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

Existing tests and future regression additions should use rollout-derived history rather than only manually constructed TUI cells. The list below is a qualification checklist, not a claim that these cases were executed or that the proposed residual fallbacks are implemented:

- legacy `exec_command` call/output pairs produce one completed command item;
- `write_stdin` contributions remain associated with the original command across process-ID reuse and exact rollback;
- structured command items take precedence, and unsupported namespaces, headerless input, missing path context and orphaned outputs retain their safe compatibility behavior;
- paginated completed command items produce the same projected shape;
- structured patch events win over raw `apply_patch` text and do not duplicate;
- existing structured `ImageView` replay renders without invoking the tool; any new raw-only Legacy fallback is covered separately;
- exact rollback removes tool items in the rolled-back range;
- malformed, orphaned, and unknown call/output pairs do not fail resume;
- Review preserves existing command previews and complete code-mode/CUA rendering, while Full retains exact stored output;
- resume and fork produce the same tool chronology for the shared prefix.

Manual verification should resume a thread containing a long file read, an
ordinary command, an `apply_patch`, image inspection, and one dedicated external
tool call. Compare the live transcript before exit with Review and Full after
resume, allowing only intentionally omitted transient states.

No compilation, tests, snapshot generation or remote CI were run for this documentation change. Executable qualification and any implementation of the residual compatibility work remain outstanding.
