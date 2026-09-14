# Codex Fork Ledger

This is the living maintenance map for capabilities carried by `fork` beyond the stabilized upstream baseline on `main`. It is a capability inventory, not a chronological changelog or a requirement to edit documentation in every feature commit.

The branch relationship is `upstream/main` → `main` (upstream plus stabilization) → `fork` (maintained downstream behavior). Inspect the downstream stack with `git log --reverse main..fork`.

Use exact semantic commit subjects as ownership anchors rather than commit hashes. Refresh those anchors after splitting, squashing, or rewording their owning commits. Describe each capability's purpose, implementation entrypoints, and upstream integration seams so its ownership remains understandable after rebasing.

## Maintained Capabilities

Capability checkpoints are added as the fork develops; this foundation does not claim that later features already exist.

## Maintenance Cadence

- Reconcile the inventory after each local release promotion and upstream rebase.
- Feature commits may update their rows when useful; a separate documentation checkpoint may consolidate multiple changes.
- Keep ledger maintenance separate from release version changes and unrelated runtime implementation.
- Include capabilities, compatibility choices, platform support, observability, and substantive efficiency improvements. Do not enumerate transient CI repair attempts.
- Remove or narrow a carry only after comparing its behavior with the upstream replacement.
- Record executable validation accurately; a history rewrite alone does not rerun CI or validate every intermediate commit.
