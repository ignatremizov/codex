# Vacuuming compacted rollout media

`codex debug rollout vacuum ROLLOUT` removes eligible inline media from compacted
replacement histories. Close the thread before running it. The command uses the
selected local Codex home (`CODEX_HOME`, or the normal default home); remote mode
is not supported. Physical vacuum is limited to validated standalone Legacy
histories: the session metadata must use Legacy history mode and have neither
`history_base` nor `subagent_history_start_ordinal`.

All Paginated rollouts are rejected, even when no descendant is currently known.
Fork and revert descendants retain immutable byte offsets into their source
rollouts; physical rewriting would invalidate those offsets. A reference-index
lookup alone cannot exclude concurrent reference creation. Physical vacuum of
Paginated history is withheld until a fenced, reference-safe implementation is
available. Append-only repairs and context sanitization still support Paginated
history. Rejection leaves rollout files and existing recovery artifacts unchanged.

`ROLLOUT` may be an absolute path or a path relative to the current directory.
Both a logical `.jsonl` path and its physical `.jsonl.zst` path are accepted,
including compressed-only rollouts. The resolved parent must be beneath this
home's `sessions` or `archived_sessions` directory. Symlinked rollout files are
rejected, as are directory links that escape the selected home.

The canonical filename must identify the same stable thread as the first
session-metadata record. A revert-shaped filename is resolved to its stable thread
identity, not its replacement rollout ID; this does not exempt it from the Legacy
restriction. Missing, mismatched, or oversized metadata that cannot establish
identity within a 1 MiB prefix is rejected before rewriting.

The command acquires the home's existing maintenance lock, then the existing
thread writer lease. Competing maintenance or an active writer causes an error
without modifying the rollout. Both guards belong to the blocking worker and
remain held through publication even if the asynchronous caller stops waiting.
Low-level vacuum APIs require the caller to establish this authority themselves.
