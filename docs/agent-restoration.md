# Agent restoration lifecycle

Persisted Multi-Agent V2 children are restored through the live control plane that owns their recorded spawn edge. The direct parent must be loaded, and its control identity and graph metadata must match the persisted owner. If the parent is absent or the ownership metadata does not match, restoration fails with a recoverable error so the caller can resume the owner chain first. It must not create a detached child control as a fallback.

The TUI may inspect stored child selections without reviving a runtime. The first live operation that requires direct input must restore the child through its owning live parent. A failed restoration preserves the caller's input and must not accidentally revive stale realtime state or silently redirect the operation.

These local graph and runtime checks apply to the native controller. A host-supplied
`AgentControl` remains the owner of its shared child-loading operation; the local
adapter must not replace it with a detached native registry or require native graph
metadata for a host-owned child. Native root recovery reuses a coherent surviving
tree, but conflicting live controller allocations fail rather than being merged.

Generic V2 `thread/resume` accepts the stored child ID or path, but the V2 child still uses the recorded owning control. Its recorded model, provider, and reasoning settings take precedence over caller choices. Any role change uses the existing allowlisted role fields; restoration is not an arbitrary whole-configuration merge. V1 keeps its caller-model behavior.

An explicit `developerInstructions` or `config.developer_instructions` on the
resume request takes precedence over that role's developer instructions, including
an explicitly empty string. Omitting the override reapplies the configured role;
global configuration defaults and inherited parent instructions are not explicit
resume overrides. Other role restrictions and recorded routing settings still apply.

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

Future-only V1 standalone adoption is a separate compatibility path for later real live turns. It does not provide cold cached terminal replay, rewrite source or nickname metadata, or retarget the native original parent for a child that is already live. V1 caller semantics remain distinct from V2 owner-controlled restoration.

Restoration must preserve lossless wait statuses and independent durable completion. A wait result describes the live operation; accepted completion follows the canonical writer flush before live installation and event enqueue. See [background subagent completion](tui-background-subagent-completion.md) for the completion and rollback contract. Neither status loss nor an ambiguous publication permits cold-repair or detached-runtime resurrection.

If a spawn-edge rollback fails, restoration is fenced for the current manager generation. Later lazy or explicit resume attempts refuse to proceed until an explicit close acknowledges the persisted `closed` state and cleanup completes. This runtime fence is not a cross-process, crash-atomic rollback guarantee.

The documentation describes the intended lifecycle and does not claim executable validation; implementation-specific checks remain deferred to the relevant test and review work.
