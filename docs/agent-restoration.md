# Agent restoration lifecycle

Explicit `/agent detach [<target>]` and V1 `close_agent` with `detach: true` create an independent root. Both use the native idle-branch handoff: reserve the branch, acknowledge writer shutdown, then atomically promote its durable aliases and remove only the selected incoming spawn edge. Runtime reload uses the unchanged UUID and authoritative new root alias instead of historical child provenance. Closed descendants stay closed, history stays intact, and old namespace refs remain reserved. Targeted detachment leaves the displayed parent in place and the independent branch unloaded for another TUI.

Model detachment must target another child in the caller's current root; resolving a foreign UUID cannot implicitly adopt it. It retains root teardown admission until the accepted handoff settles, rechecks ownership inside the worker, and refuses busy branches without interrupting them. Response replay flags are incompatible with this mode. Existing V2 tool surfaces retain their own lifecycle operations; user-targeted detachment supports native branches from either agent version.

On resume, validated current aliases select both the live session namespace and its current parent. A promoted root therefore retains its own namespace and no parent even when its rollout preserves the former root UUID and spawn provenance.

Reattachment uses explicit `resume_agent` or `/agent resume <uuid>` adoption. An idle independent root can be durably handed off without a separate close; active or queued branch work rejects the operation. An ordinary child that is live under another root still requires close. Generic `thread/resume` preserves current ownership and never implicitly detaches a child. These ownership changes require the native durable graph; host controllers keep their existing lifecycle contract.

Model-requested ownership changes have explicit TUI lifecycle rows: `Detaching`/`Detached … from Main` and `Reattaching`/`Reattached … to Main`. The target uses its caller-root identity throughout; an unknown target uses its UUID until adoption assigns an alias. Reattachment completion shows the assigned ref in the title, the task path in the details, and `Initializing` or `Idle` as appropriate. Response subscriptions remain secondary details. The detach row carries the independent-resume guidance, while legacy or externally initiated detachments retain a standalone notice. Ordinary close/resume keeps its existing labels. The typed action and captured identities survive transcript replay.

Persisted Multi-Agent V2 children are restored through the live control plane that owns their recorded spawn edge. The direct parent must be loaded, and its control identity and graph metadata must match the persisted owner. If the parent is absent or the ownership metadata does not match, restoration fails with a recoverable error so the caller can resume the owner chain first. It must not create a detached child control as a fallback.

The TUI may inspect stored child selections without reviving a runtime. The first live operation that requires direct input must restore the child through its owning live parent. A failed restoration preserves the caller's input and must not accidentally revive stale realtime state or silently redirect the operation.

These local graph and runtime checks apply to the native controller. A host-supplied
`AgentControl` remains the owner of its shared child-loading operation; the local
adapter must not replace it with a detached native registry or require native graph
metadata for a host-owned child. Native root recovery reuses a coherent surviving
tree, but conflicting live controller allocations fail rather than being merged.

Generic V2 `thread/resume` accepts the stored child ID or path, but the V2 child still uses the recorded owning control. Native child restoration reapplies the recorded model, provider, reasoning, and service-tier settings after allowlisted role restrictions, including the V1 controlled-child path. Caller defaults and later root-tier changes do not silently replace a saved per-spawn selection. Restoration is not an arbitrary whole-configuration merge, and the V1 and V2 ownership/adoption rules remain distinct.

An explicit `developerInstructions` or `config.developer_instructions` on the
resume request takes precedence over that role's developer instructions, including
an explicitly empty string. Omitting the override reapplies the configured role;
global configuration defaults and inherited parent instructions are not explicit
resume overrides. Other role restrictions and recorded routing settings still apply.

Owned-agent model requests project that effective value into harness-classified developer
instruction fragments, including when a resumed or history-forked thread retains
an older initial-context bundle. Historical audit records are not rewritten.
Managed policy, client-authored developer messages and unclassified legacy text
are not replaced by this projection. An absent resolved value leaves historical
instructions unchanged; an explicitly empty value removes the classified fragments.
Root sessions, snapshot-only workers, Guardian review sessions and compaction
decoders keep their existing history behavior.

Workspace settings recovered from history are not an explicit request to retarget
the live owner's executor. Child restoration preserves that captured attachment.
An explicitly supplied `cwd` or `runtimeWorkspaceRoots` remains a host-boundary
override; it must not silently reinterpret host paths in a remote workspace.

The options-based `ThreadManager::start_thread` resume path uses the same ownership
and publication checks as the dedicated resume API. Recovering a root while one
of its children remains live reuses that child's existing control plane; it does
not create a second registry for the same durable owner. Cold-root options,
including its host-supplied instruction provider, remain intact. Warm attachment
returns the existing runtime without replacing its provider, and a live Guardian
reviewer or a thread being unloaded cannot be attached through this path.

Unloading a runtime does not allocate a new agent identity. If its registration
is retained, restoration updates canonical metadata under the existing
registration transaction and commits it with runtime publication. It preserves
the submission gate and accepted task metadata, consumes no additional agent
slot, and still fails if that registration changes during restoration. A real
close releases the slot; a subsequent new spawn must be able to use it.

Legacy spawn edges can be indexed after the owning root's alias namespace already exists. If restoration finds a persisted ancestry chain but no alias for its target, it explicitly backfills missing graph-backed aliases in one transaction. This keeps closed edges closed and existing refs, names, and ownership unchanged; it is not an activation or a new permission grant. Ordinary namespace initialization keeps its one-time migration scan instead of rescanning the tree for every spawn.

Future-only V1 standalone adoption is a separate compatibility path for later real live turns. It does not provide cold cached terminal replay, rewrite source or nickname metadata, or retarget the native original parent for a child that is already live. V1 caller semantics remain distinct from V2 owner-controlled restoration.

Restoration must preserve lossless wait statuses and independent durable completion. A wait result describes the live operation; accepted completion follows the canonical writer flush before live installation and event enqueue. See [background subagent completion](tui-background-subagent-completion.md) for the completion and rollback contract. Neither status loss nor an ambiguous publication permits cold-repair or detached-runtime resurrection.

If a spawn-edge rollback fails, restoration is fenced for the current manager generation. Later lazy or explicit resume attempts refuse to proceed until an explicit close acknowledges the persisted `closed` state and cleanup completes. This runtime fence is not a cross-process, crash-atomic rollback guarantee.

The documentation describes the intended lifecycle and does not claim executable validation; implementation-specific checks remain deferred to the relevant test and review work.
