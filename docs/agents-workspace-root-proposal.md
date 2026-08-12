# Workspace-root AGENTS.md discovery

## Status

Proposal.

## Summary

Add a dedicated `.codex-agents-root` marker that lets one `AGENTS.md` apply across a multi-repository workspace.

When Codex starts below this marker, AGENTS discovery uses the marked directory as its outer boundary and continues collecting instructions along the ancestor chain down to the selected working directory. Repository and worktree `AGENTS.md` files therefore remain more specific layers beneath the shared workspace instructions.

If no `.codex-agents-root` exists in the ancestor chain, discovery retains its current nearest-project-root behavior, including the default `.git` marker.

This is an instruction-discovery feature only. It does not change project trust, project-local configuration, filesystem permissions, workspace roots, or Git behavior.

## Motivation

Codex currently discovers project instructions from the nearest configured project root to the working directory. With the default `project_root_markers = [".git"]`, this works naturally for one repository:

```text
repository/
├── .git/
├── AGENTS.md
└── internal/
    └── AGENTS.md
```

A multi-repository product workspace commonly has another useful layer:

```text
workspace/
├── AGENTS.md
├── frontend/
│   ├── .git/
│   └── AGENTS.md
├── service-a/
│   ├── .git/
│   └── AGENTS.md
└── worktrees/
    └── service-a-feature/
        ├── .git
        └── AGENTS.md
```

The workspace file owns shared architecture, tooling, coordination, and cross-repository conventions. Each repository file owns its local build, test, layout, and implementation guidance.

Starting Codex inside `service-a/` currently stops discovery at `service-a/.git`, so the workspace instructions are absent. The available workarounds all have material drawbacks:

- Copying shared instructions into every repository creates drift and wastes context.
- Putting product-specific instructions in `$CODEX_HOME/AGENTS.md` applies them to unrelated workspaces.
- Asking the model to read the workspace file consumes a tool call and model output, weakens prompt caching, and relies on the model repeating bootstrap work.
- Recursively scanning sibling repositories would load unrelated and potentially conflicting instructions.
- Adding `.git` and a workspace marker to one `project_root_markers` list does not solve the problem because the nearer repository `.git` wins before discovery reaches the outer workspace marker.

The harness should assemble deterministic instruction context before inference.

## Goals

- Let an explicitly marked multi-repository workspace provide one shared `AGENTS.md`.
- Continue loading repository, worktree, and nested `AGENTS.md` files from the same ancestor chain.
- Preserve root-to-leaf precedence so more specific instructions remain later in model context.
- Require no model-authored filesystem read.
- Require no per-developer global configuration for the standard marker.
- Preserve current behavior in every workspace that does not contain the marker.
- Avoid treating sibling or descendant instruction files as applicable.

## Non-goals

- Changing how Codex determines project trust.
- Changing `.codex/config.toml` discovery or precedence.
- Changing sandbox permissions or writable workspace roots.
- Treating `.codex-agents-root` as a Git or source-control root.
- Recursively discovering every `AGENTS.md` below a workspace.
- Adding an include language to `AGENTS.md`.
- Automatically sharing instructions between unrelated working directories.
- Defining broader semantics for a generic Codex workspace.

## Proposed marker

The built-in marker is:

```text
.codex-agents-root
```

The name is intentionally narrow:

- `.codex-` identifies the consuming harness.
- `agents` limits the behavior to AGENTS instruction discovery.
- `root` describes the outer boundary of the applicable ancestor chain.

Names such as `.codex-root` or `.codex-workspace` imply broader configuration, trust, or permission semantics that this proposal does not introduce.

The recommended representation is an empty file committed or created at the shared workspace root:

```text
workspace/
├── .codex-agents-root
└── AGENTS.md
```

Codex only uses the marker's presence. Its contents are ignored.

## Discovery algorithm

For each selected environment and logical working directory:

1. Search the working directory and each ancestor for `.codex-agents-root`.
2. If one or more markers exist, select the nearest marked ancestor.
3. Otherwise, determine the project root using the existing `project_root_markers` behavior.
4. Build the ordered directory chain from the selected root through the working directory.
5. In each directory, retain the existing candidate precedence:
   1. `AGENTS.override.md`
   2. `AGENTS.md`
   3. configured fallback filenames
6. Concatenate discovered instructions from root to working directory using the existing byte budget and provenance model.

In pseudocode:

```text
agents_root = nearest_ancestor_with(".codex-agents-root")

if agents_root is absent:
    agents_root = nearest_ancestor_with(project_root_markers)

if agents_root is absent:
    search_dirs = [cwd]
else:
    search_dirs = ancestors_from(agents_root, cwd)

instructions = first_candidate_in_each(search_dirs)
```

The workspace marker search is a separate first pass. It must not be implemented by prepending `.codex-agents-root` to `project_root_markers`, because marker ordering only resolves candidates at the same ancestor; it does not override a nearer `.git`.

## Examples

### Multi-repository workspace

```text
workspace/
├── .codex-agents-root
├── AGENTS.md                         "shared"
└── worktrees/
    └── payments-feature/
        ├── .git
        ├── AGENTS.md                 "payments"
        └── internal/
            └── ledger/
                ├── AGENTS.md         "ledger"
                └── posting.go        cwd
```

Codex injects:

```text
shared
payments
ledger
```

The nested `.git` does not truncate AGENTS discovery because the explicit agents root has precedence for this one purpose.

### Ordinary repository

```text
repository/
├── .git/
├── AGENTS.md
└── src/                              cwd
```

With no `.codex-agents-root`, Codex injects the repository `AGENTS.md` exactly as it does today.

### Nested agents roots

```text
outer/
├── .codex-agents-root
├── AGENTS.md
└── isolated/
    ├── .codex-agents-root
    ├── AGENTS.md
    └── repo/                         cwd
```

The nearest marker, `isolated/.codex-agents-root`, wins. The outer file does not apply. This lets a nested workspace establish an intentional instruction boundary.

### Siblings remain out of scope

```text
workspace/
├── .codex-agents-root
├── AGENTS.md
├── service-a/
│   └── AGENTS.md
└── service-b/                        cwd
    └── AGENTS.md
```

Codex injects `workspace/AGENTS.md` and `service-b/AGENTS.md`. It does not inspect or inject `service-a/AGENTS.md`.

## Precedence and scope

This proposal does not change the existing instruction hierarchy:

- System, developer, and direct user instructions retain their established precedence.
- User-level `$CODEX_HOME` instructions retain their established position.
- Project instructions are ordered from the selected AGENTS root toward the working directory.
- A deeper `AGENTS.md` can specialize or override broader workspace guidance.
- `AGENTS.override.md` continues replacing `AGENTS.md` only within the same directory.

The marker affects discovery scope, not semantic precedence.

## Context and caching behavior

The discovered files flow through the existing `LoadedAgentsMd`, provenance, byte-budget, environment snapshot, and world-state paths. The model receives the assembled instructions as harness-provided context and does not need to call a filesystem tool.

The implementation should preserve the existing behavior that unchanged AGENTS state is reused rather than appended as fresh conversational content every turn. Changing the selected working directory or environment may produce a new applicable instruction chain through the existing refresh mechanism.

## Compatibility and safety

The change is opt-in by filesystem marker:

- Existing repositories without `.codex-agents-root` are unaffected.
- Existing `project_root_markers` configuration remains authoritative when no agents marker is found.
- Existing fallback filenames, overrides, byte limits, logical-path handling, and multi-environment provenance remain unchanged.
- The marker cannot broaden sandbox access or project trust; it only broadens which ancestor instruction files are read.

A repository cannot cause Codex to read arbitrary sibling instructions merely by containing the marker. Discovery still follows one ancestor chain. A marker committed inside an ordinary repository is equivalent to selecting that repository as the AGENTS root.

## Implementation outline

The implementation should remain localized to AGENTS discovery:

1. Add a constant for `.codex-agents-root`.
2. In `codex-rs/core/src/agents_md.rs`, search for the dedicated marker before resolving the existing project root.
3. Reuse the existing ancestor-chain construction and candidate probing after selecting the root.
4. Update `docs/agents_md.md` with the public marker contract.
5. Add focused discovery tests and one integration test proving the assembled model-visible instruction order.

No `ConfigToml` field or generated configuration schema change is required for the initial implementation. Configurable alternate agents-root marker names can be considered later if a concrete interoperability need appears.

## Test plan

### Discovery tests

- A workspace marker above a nested `.git` loads both workspace and repository instructions.
- A workspace marker at the working directory loads that directory's instructions.
- The nearest of two workspace markers wins.
- Without a workspace marker, the nearest configured project root remains the boundary.
- An empty `project_root_markers` list still disables project-root traversal when no workspace marker exists.
- Sibling `AGENTS.md` files are not discovered.
- `AGENTS.override.md` and fallback filename precedence remain unchanged at every level.
- Root-to-working-directory ordering and the shared byte budget remain unchanged.
- Logical symlink ancestry retains the existing behavior.

### Integration test

Start a thread inside a Git worktree nested under a marked workspace and assert that the model-visible AGENTS section contains:

1. workspace instructions;
2. worktree instructions;
3. nested instructions, when present;

in that order and exactly once.

The test should also prove that an adjacent repository's instructions are absent.

## Alternatives considered

### Configure `project_root_markers = [".codex-agents-root", ".git"]`

Rejected. Discovery selects the first ancestor containing any configured marker. A nested `.git` is encountered before the outer workspace marker, regardless of marker-list order.

### Replace `.git` with `.codex-agents-root`

Rejected. A global configuration change would alter project-root behavior for unrelated workspaces and require every developer to maintain matching local configuration.

### Put shared guidance in `$CODEX_HOME/AGENTS.md`

Rejected. Product-specific instructions would affect every Codex workspace owned by that user.

### Ask the model to read a shared document

Rejected. Bootstrap context should be assembled by the harness. Repeated reads consume tool calls and output tokens, reduce prompt-cache stability, and can be skipped or applied too late.

### Copy shared guidance into each repository

Rejected. Duplicate instructions drift, increase context, and obscure which copy is authoritative.

### Recursively scan the workspace

Rejected. Instruction scope is ancestor-based. Fan-out would load unrelated sibling guidance and make precedence ambiguous.

### Add AGENTS include directives

Deferred. Includes require path-resolution, cycle, depth, byte-budget, provenance, and remote-filesystem semantics. A workspace boundary solves the current use case with a smaller and more predictable contract.

## Expected result

A developer can mark one multi-repository workspace and maintain:

```text
workspace/AGENTS.md
repository/AGENTS.md
```

Codex automatically injects both files when started inside that repository, with shared instructions first and repository-specific instructions second. Other projects continue using their existing `.git`-bounded discovery without configuration or behavior changes.
