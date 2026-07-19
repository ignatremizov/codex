# Compacted media and canonical history

Compaction removes retained image payloads, including file-backed images and structured tool
output images, from its replacement history. It preserves bounded omission text and usable local
image paths for one compaction window. Those paths expire when the next successful compaction
replaces that prefix. Current-window images remain available to the compactor before replacement.
Reserved image wrappers and omission fragments are retained atomically within the text budget.

This policy is unconditional; `compaction_image_budget` is no longer a feature switch.

## Resume and fork

Old compacted histories are sanitized during reconstruction. The canonical rollout receives an
append-only representation-repair checkpoint; original audit records are not rewritten. The
checkpoint records the sanitized prefix separately from the unsummarized suffix. Fork filtering
updates that boundary without discarding retained envelope metadata.

A repair is not semantic compaction: it does not advance the context window, run compaction
hooks, or satisfy the bounded model-context scanner's semantic-checkpoint requirement. Retained
authorization context, Guardian policy, model context, settings, and cumulative usage survive
reconstruction. Active-context token estimates are recomputed when the old count no longer
describes the repaired history.

Required repairs commit through the existing tracked history-publication worker before installing
live history. Cancellation of the waiter does not cancel the accepted write or release its
persistence permit. An uncertain canonical failure requires reload rather than replaying the
append. Media-free historical checkpoints can receive a best-effort policy certification.

SQLite history is a derived view. Canonical append success remains success if projection fails;
later projection retries do not append the same canonical batch again. When vacuum changes decoded
record offsets, projection rebuild replaces its rows and cursor in one transaction. A failed
replacement leaves the prior projection intact. Compressed rollouts use decoded JSONL offsets.

## Explicit physical cleanup

Only the explicit [compacted-media vacuum command](compacted-media-vacuum.md) rewrites old
compacted payloads on disk, and only for closed standalone Legacy rollouts. Paginated physical
vacuum is refused because fork and revert histories retain immutable offsets into their sources;
their normal append-only repairs and context sanitization remain supported.
Ordinary resume, model-context reads, and canonical Legacy-to-Paginated
migration preserve surviving audit history. Read-only recovery selects only a validated,
manifest-authorized backup; writer-authorized recovery owns any restoration.
