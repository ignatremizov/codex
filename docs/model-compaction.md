# Model-requested and active user compaction

A root thread or native subagent can call `compact {}` to request its own normal
`/compact` operation. It runs after the current sampling request and all tools in
that batch have settled. The thread then resumes the same task automatically,
with the same turn identity; another user prompt or `/continue` is unnecessary.

Use this when the user has changed tasks, or when lengthy investigation and
screenshot analysis can be reduced to the findings needed to finish the task.
The normal compactor sees the recorded user input and completed tool results.
Preserve useful observations in the conversation before calling; the
tool does not accept a replacement history or a target thread.

The tool follows the configured local or remote compaction implementation,
retention policy, hooks, and visible compaction lifecycle. In token-budget mode,
the existing `/compact` policy starts a fresh context window without generating
a summary. Save necessary context before requesting that reset. Compaction
tool instructions describe only the active mode: standard sessions receive
summarizing guidance, and token-budget sessions receive reset and context-saving
guidance. Decoders and private helpers cannot recursively compact their own input.

Set `tools.compact.enabled = false` to remove the model-facing tool while retaining
user `/compact`. Temporary structured requests disable the tool to preserve
their existing isolation.

## `/compact` during work

While a regular turn is running, `/compact` schedules compaction at its next safe
sampling/tool boundary. It does not interrupt the current tools or replace the
turn. Once compaction finishes, work continues automatically. The TUI confirms
the request immediately and shows compaction progress when it actually starts.
Outstanding approvals or tool replies must settle before that boundary.

Repeated requests before the boundary share one compaction. A request arriving
while compaction is already underway is satisfied by that compaction if it
succeeds. Explicit requests use the current history instead of an older prepared
background summary. A request racing turn finalization remains owned by that
turn and runs before its continuation.

On an idle or already interrupted thread, `/compact` retains its standalone
behavior and does not start task inference afterward. An ongoing review must
finish before user compaction can run.

## Failure and cancellation

Successful inline compaction returns to the regular sampling loop, including
ordinary tool execution and newly accepted user input. A compaction error follows
the existing failure path; it is surfaced rather than repeatedly restarting work.
Interrupting the originating turn discards its pending request. That request
cannot compact a later turn, its parent, or another thread.

`compact` is a direct model tool in every supported tool mode, including Code
Mode. It is excluded from nested Code Mode execution so a background cell cannot
request compaction for an unrelated active turn.
