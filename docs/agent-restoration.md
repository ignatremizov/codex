# Agent restoration lifecycle

Persisted Multi-Agent V2 children are restored through the live control plane that owns their recorded spawn edge. The direct parent must be loaded, and its control identity and graph metadata must match the persisted owner. If the parent is absent or the ownership metadata does not match, restoration fails with a recoverable error so the caller can resume the owner chain first. It must not create a detached child control as a fallback.

The TUI may inspect stored child selections without reviving a runtime. The first live operation that requires direct input must restore the child through its owning live parent. A failed restoration preserves the caller's input and must not accidentally revive stale realtime state or silently redirect the operation.

Generic V2 `thread/resume` accepts the stored child ID or path, but the V2 child still uses the recorded owning control. Its recorded model, provider, and reasoning settings take precedence over caller choices. Any role change uses the existing allowlisted role fields; restoration is not an arbitrary whole-configuration merge. V1 keeps its caller-model behavior.

Future-only V1 standalone adoption is a separate compatibility path for later real live turns. It does not provide cold cached terminal replay, rewrite source or nickname metadata, or retarget the native original parent for a child that is already live. V1 caller semantics remain distinct from V2 owner-controlled restoration.

Restoration must preserve lossless wait statuses and independent durable completion. A wait result describes the live operation; accepted completion follows the canonical writer flush before live installation and event enqueue. See [background subagent completion](tui-background-subagent-completion.md) for the completion and rollback contract. Neither status loss nor an ambiguous publication permits cold-repair or detached-runtime resurrection.

If a spawn-edge rollback fails, restoration is fenced for the current manager generation. Later lazy or explicit resume attempts refuse to proceed until an explicit close acknowledges the persisted `closed` state and cleanup completes. This runtime fence is not a cross-process, crash-atomic rollback guarantee.

The documentation describes the intended lifecycle and does not claim executable validation; implementation-specific checks remain deferred to the relevant test and review work.
