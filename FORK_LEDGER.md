# Codex Fork Ledger

This is the living maintenance map for capabilities carried by `fork` beyond its audited upstream integration base. It is a capability inventory, not a chronological changelog or a requirement to edit documentation in every feature commit.

The 0.160 integration is pinned to upstream source commit `cb7799623b2241f536d799f2d46259148fb987ce`, including its maintenance backports. The upstream release-notes/version commit `a956835d020762cb2b570053af06f643a11c0ecc` is deliberately excluded. Neither rolling `upstream/main` nor local `main` defines the integration base. Keep release-version changes at the final release owner.

The replay manifest inventories the 130 original downstream commits in `f1b21bb2931f86819e32df0cafa1bf69570d84ee..ac0f63abe470b8fa83d7852db7e5d39282c88434`. Reviewed dispositions distinguish retained owners, target adaptations, and explicitly deferred repairs assigned to their defining feature. After integration completes, inspect the resulting downstream stack with `git log --reverse cb7799623b2241f536d799f2d46259148fb987ce..fork`; during replay, `fork` still names the original branch tip. The complete pre-rebase ledger remains the policy-preservation checklist; a partially replayed foundation is not evidence that its later capabilities were dropped or that the new source has passed CI.

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
