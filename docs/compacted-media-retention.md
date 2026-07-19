# Compacted media and canonical history

Compaction removes retained image payloads, including file-backed images and structured tool
output images, from its replacement history. It preserves bounded omission text and usable local
image paths for one compaction window. Those paths expire when the next successful compaction
replaces that prefix. Current-window images remain available to the compactor before replacement.
Reserved image wrappers and omission fragments are retained atomically within the text budget.

This policy is unconditional; `compaction_image_budget` is no longer a feature switch.

## Private Guardian evidence

A reused Guardian reviewer can receive retained REPL screenshots again after its own
compaction removes them. Screenshot candidates come from the existing bounded private
evidence store; already-admitted REPL text is not appended again. Image deduplication
uses the reviewer's history at final input selection, after pre-turn compaction,
rather than the earlier history snapshot used to prepare the review.

Feature eligibility, model image support, prepared-image identity and request/image
budgets still govern admission. Evicted screenshots are not reconstructed, and this
private replay does not put screenshot payloads into the parent's conversation.

Ordinary fork and replay snapshots preserve the reviewer's unsummarized suffix evidence. A real
compaction separately sanitizes its retained reviewer checkpoint. Historical Guardian-only media
rewrites still require canonical repair; the surrounding envelope identity and rollback metadata
remain intact, while changed source-content completeness claims are invalidated.

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

Self-contained semantic checkpoints retain their explicit resume metadata, including an explicitly
absent previous-turn setting. Frozen repair companion records cannot replace that authority with
older settings. A later completed turn still supplies its own newer settings and start identity.

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
