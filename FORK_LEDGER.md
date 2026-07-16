# Codex Fork Ledger

This is the living maintenance map for capabilities carried by `fork` beyond its audited upstream integration base. It is a capability inventory, not a chronological changelog or a requirement to edit documentation in every feature commit.

The 0.156.1 integration is pinned to upstream source commit `f1b21bb2931f86819e32df0cafa1bf69570d84ee`, including its catalog backport. Neither the rolling `upstream/main` branch nor local `main` defines this integration base. Keep release-version changes at the final release owner.

The replay manifest inventories the original non-merge commits in `3d2ee51ca2d5db578f328aa75e20aa22c0197c9a..4878a2c6dbc478457d59e0ffb2845656d085aa5e`. Its reviewed dispositions distinguish retained owners, explicit drops, and new integration safety commits. After integration completes, inspect the resulting downstream stack with `git log --reverse f1b21bb2931f86819e32df0cafa1bf69570d84ee..fork`; during replay, `fork` still names the original branch tip.

Use exact semantic commit subjects as ownership anchors rather than commit hashes. Refresh those anchors after splitting, squashing, or rewording their owning commits. Describe each capability's purpose, implementation entrypoints, and upstream integration seams so its ownership remains understandable after rebasing.

## Maintained Capabilities

Capability checkpoints are added as the fork develops; this foundation does not claim that later features already exist.

### Built-in collaboration role schemas

The collaboration tool plan exposes `agent_type` whenever built-in or configured roles are
resolvable. V2 collaboration tools retain their fork-owned bundled parameter schemas, encrypted
argument markers, namespaces, and runtime forwarding even when model catalogs provide incompatible
parameter overrides; catalog descriptions remain independently overridable where supported.
Entrypoints are `core/src/agent/role.rs`, `core/src/tools/spec_plan.rs`, and
`core/src/tools/multi_agent_tool.rs`. Focused schema and request-level coverage lives in the
corresponding core tool tests and scenario fixtures.

Explicit child model overrides resolve against the loaded catalog regardless of the selected
multi-agent runtime tag; only the concise picker description is limited to five visible models.
Unknown models remain rejected with the complete loaded catalog listed, while child runtime exposure
continues to follow the resolved turn version.

## Maintenance Cadence

- Reconcile the inventory after each local release promotion and upstream rebase.
- Feature commits may update their rows when useful; a separate documentation checkpoint may consolidate multiple changes.
- Keep ledger maintenance separate from release version changes and unrelated runtime implementation.
- Include capabilities, compatibility choices, platform support, observability, and substantive efficiency improvements. Do not enumerate transient CI repair attempts.
- Remove or narrow a carry only after comparing its behavior with the upstream replacement.
- Record executable validation accurately; a history rewrite alone does not rerun CI or validate every intermediate commit.
