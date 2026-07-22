# Legacy rollout migration

Migration converts a legacy rollout into canonical paginated history by replaying
the complete surviving transcript. This applies equally to ordinary sessions and
subagents. Historical rollback is normalized by the existing rollback planner:
removed instruction turns and their dependent records are excluded, while
completion evidence belonging to surviving turns remains attributed to those
turns. Compatible historical records are decoded tolerantly; malformed or
unsupported records retain the existing decoder's handling.

Exact rollback cutoffs use the full canonical decoded record vector, before
compatibility normalization, metadata relocation, or ordinal assignment.
Migration scans for exact markers without changing managed migration or recovery
files; compressed input can use an anonymous decoded snapshot. For exact
sources only, it loads canonical records and their physical source positions,
including valid retired events, oversized records, and delayed/copied metadata.
Compatibility-only records are retained or removed using those source positions.
Canonical decoding also performs the same embedded ghost-snapshot cleanup as
cold resume. Surviving records preserve their original timestamps and media.
Exact ranges are applied before the existing count-only rollback planner;
exact markers are not applied again as instruction-count rollbacks.

This exact-only pass costs O(decoded source) memory. Count-only migration keeps
its streaming replay path; detection requires an additional streaming read and
space for one uncapped source line. An invalid or unprovable exact coordinate is
rejected under writer exclusion before changing the source, projection, journal,
or staging files, without running unpublished-migration cleanup.
Detection tolerates invalid UTF-8 records on count-only sources, as the existing
compatibility parser does. Exact sources require canonical UTF-8 decoding and
are rejected during preflight if that decoding fails. A cutoff at or after its
own marker is also rejected rather than silently treated as an empty range.

Canonical storage and model context serve different purposes. The canonical
rollout retains historical messages, tool records, and compaction checkpoints.
Paginated model-context loading independently selects a usable checkpoint and its
required suffix. Without a provably usable cutoff, it falls back to full replay.
Migration never publishes that bounded model-context selection as the canonical
transcript.

The rebuildable `fork_thread_history_projection_state` cache adds
`projection_version INTEGER NOT NULL DEFAULT 0`. Version 0 rows are rebuilt even
when their byte/ordinal checkpoint already matches EOF. Version 1 certifies exact
rollback filtering and is published atomically with all derived rows and both
projection checkpoints; a failed rebuild retains the previous data and version.
This is a local cache implementation version, not a canonical rollout format or
released database migration version.

## Inherited history and audit access

Legacy migration does not infer where a child's inherited context ends. In
particular, it does not label the entire migrated transcript as inherited by
assigning an end-of-file boundary. Surviving historical child records therefore
remain available to paginated turn and item readers. A legacy child that copied
parent records can expose those copied records too; migration does not invent
new attribution.

Ordinary copied legacy forks retain their `forked_from_id` and copied records.
Embedded ancestor session metadata is not the child's authoritative header.

An authoritative legacy header containing `history_base` or
`subagent_history_start_ordinal` is rejected before source, journal, staging, or
projection changes. Canonicalization changes ordinals, and there is no proven
translation for these legacy lineage fields. Such a failure preserves the
existing files for investigation rather than clearing the fields or copying a
stale boundary into the output.

Already-paginated histories keep their recorded lineage and inherited-context
boundaries unchanged, including during interrupted-publication recovery.

## Publication and interrupted migration

The original legacy rollout remains authoritative while migration writes and
synchronizes a full canonical stage. SQLite projection must cover that stage's
complete byte length and ordinal range before atomic replacement. The existing
source length and modification-time check rejects a source changed during
staging. Migration holds the existing maintenance and writer locks.

If migration is interrupted before replacement, the source still identifies
itself as legacy. A retry discards the stale projection and regenerates staging
from that source. An older migrator's bounded stage, rewritten header stage, or
compressed temporary is not an authoritative recovery source. Pending journals
also cause startup migration to reconsider the affected rollout instead of
relying solely on the normal startup cursor.

After replacement, the synchronized full canonical rollout is the recovery
source. The pending journal allows a subsequent run to reconstruct an incomplete
SQLite projection and finish metadata work. Recovery does not rewrite genuine
paginated lineage. The journal is retired only after completion; the raw legacy
file is not retained as an additional backup after successful atomic replacement.

This change prevents new loss during migration. It cannot restore historical
records already discarded by a previously published bounded replacement.
An old pending journal is only a recovery marker, not a backup or proof that the
published transcript was complete. Recovering that publication's projection
does not recover missing history.
