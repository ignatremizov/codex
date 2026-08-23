# TUI local app-server handoff and fallback

Status: deferred proposal

Related work:

- [TUI user-controlled multi-agent dispatch](tui-agent-control.md)
- [TUI resume tool history](tui-resume-tool-history.md)
- [TUI alternate-screen behavior](tui-alternate-screen.md)

## Summary

Codex TUI can run against an embedded app-server or a shared local app-server daemon. The
daemon-backed mode enables the daemon-wide `/agents` dashboard and can reduce duplicated
app-server infrastructure across multiple TUI processes. Today, however, the app-server target is
selected at startup:

- `/agents` is unavailable to an embedded session;
- starting the local daemon from that dialog does not move the current session to it; and
- losing a daemon connection terminates connected TUI sessions instead of restoring each one
  through an embedded app-server.

The TUI should support an explicit, reversible handoff between its embedded app-server and the
local daemon. If a connected local daemon exits, the TUI should retain its interface state and
silently restore the same root thread graph through a new embedded app-server.

This is a local-host feature. A remote app-server owns a potentially different workspace,
environment, credential, and permission boundary and must not silently fall back to the local
machine.

## Goals

- Let an existing embedded TUI move its current root thread graph to the shared local daemon.
- Let a daemon-backed TUI move its graph back to an embedded app-server.
- Recover automatically into embedded mode after an unambiguous local-daemon process failure.
- Keep the current thread UUID, durable agent graph, aliases, roles, nicknames, model context, and
  queued work.
- Preserve the TUI's rendered transcript, composer, focus, scroll position, selected agent, and
  overlays rather than rebuilding the visible interface through the ordinary cold-resume path.
- Use the existing per-thread writer lock as the authority that prevents competing live writers.
- Keep agent concurrency accounting scoped to each root graph after handoff.
- Make every handoff, fallback, conflict, and interrupted turn visible and auditable.
- Allow `/agents` to operate after a successful connection without forcing the user to restart
  Codex.

## Non-goals

- Seamlessly moving an actively sampling model turn or a running subprocess between processes.
- Falling back locally from a remote app-server connection.
- Replacing the per-thread writer lock with a second ownership or lease system.
- Treating every stored rollout as loaded in the daemon-wide `/agents` dashboard.
- Adding a daemon-wide aggregate agent limit as part of handoff.
- Fixing all cold-resume transcript reconstruction gaps. A live TUI handoff should avoid invoking
  visible transcript reconstruction, but ordinary `codex resume` remains separate work.

## Current ownership and limits

The local daemon is a shared process, but multi-agent admission is not daemon-global.

Each root session owns one `AgentControl`. Its registry, response-observation state, V2 residency,
and V2 execution limiter are shared with all descendants in that root graph. A different root
loaded by the same daemon receives a different control plane and an independent configured limit.

Consequently:

- V1 counts live descendant agents within one root control plane.
- V2 reserves capacity for the root and bounds resident or executing descendants within that root
  control plane.
- moving a graph between embedded and daemon-backed app-servers must not reset its accounting or
  combine it with another root's accounting;
- a daemon containing many roots can exceed any one root's configured agent count in aggregate.

A future global daemon resource budget may be useful, but it must be an independently named and
reported limit rather than silently changing `agents.max_concurrent_threads_per_session`.

## Proposed TUI surface

Introduce a small server-status command surface:

```text
/server
/server connect local
/server disconnect
```

`/server` shows:

- `embedded`, `local daemon`, or `remote`;
- the local daemon endpoint and process health when applicable;
- whether a handoff or recovery is in progress;
- the current root thread ID; and
- actions valid from the current state.

`/server connect local` starts the configured local daemon when it is absent, then moves the
current root graph to it. `/server disconnect` moves a local-daemon graph back into the current TUI
process. It is unavailable for remote app-server sessions.

The embedded `/agents` unavailable dialog should also offer:

1. `Move this session to shared server`
2. `Start background server only`
3. `Return to this session`

Starting the server alone retains its current behavior and does not move the session.

The exact command name may be reconciled with any general connection-management surface that lands
before implementation. The required product distinction is between starting a daemon and
transferring the current session to it.

## Handoff boundary

A root session and its owned descendants move as one graph. Moving only the currently displayed
thread would split one `AgentControl`, its aliases, response observations, and queued turns across
two app-server processes.

The first implementation should admit a handoff only when:

- no thread in the root graph has an active model turn;
- no graph-owned unified-exec process is running;
- no lifecycle mutation or queued-turn admission is in progress; and
- every rollout and durable graph update has been flushed.

The TUI can wait for ordinary work to finish or ask the user to interrupt it. It must not silently
interrupt active work merely to optimize memory.

Later work may support draining a busy graph before handoff, but migrating active model streams,
subprocess pipes, or approval requests is unnecessary.

## Embedded to local-daemon flow

```text
user: /server connect local
        |
        +-- start or probe local daemon
        +-- resolve the current root and complete owned graph
        +-- reject or wait while the graph is busy
        +-- freeze new graph admission
        +-- flush rollouts, aliases, queues, and response observations
        +-- detach the graph from the embedded runtime
        +-- release its per-thread writer locks
        +-- ask the daemon to resume the same root graph
        +-- daemon acquires every writer lock and restores graph ownership
        +-- rebind TUI app-server requests and notifications
        +-- retain all existing TUI presentation state
        +-- unfreeze admission
```

The destination daemon must restore the root and every still-live descendant using durable
parentage and alias metadata. A hot handoff is not a user-initiated cold resume or fork:
process-lifetime response observations and queued work that were explicitly included in the
handoff snapshot should remain active.

If destination admission fails after the source releases ownership, the TUI should reacquire the
writer locks through a new embedded runtime and resume locally. A competing-writer error is
recoverable and must identify the thread that remains owned elsewhere.

## Local-daemon to embedded flow

Explicit disconnect uses the same graph-wide protocol in reverse:

1. verify that the graph is idle;
2. freeze graph admission in the daemon;
3. flush durable state;
4. detach the graph and release writer locks;
5. start an embedded app-server;
6. resume the same graph and acquire its writer locks;
7. rebind the TUI; and
8. retain the existing interface state.

Disconnecting one graph must not stop the daemon or affect other connected TUI sessions.

## Automatic fallback after daemon loss

An operating-system-backed thread writer lock prevents two active rollout writers:

- if the daemon process died, the operating system releases its locks and embedded restoration can
  proceed;
- if the daemon is merely unreachable but still alive, its writer locks remain held and embedded
  restoration receives an ownership conflict;
- a restarted daemon cannot reload a thread already restored by an embedded TUI because that TUI
  owns the same lock.

After a local-daemon transport closes, the TUI should:

1. retain the complete in-memory interface state;
2. determine whether the local daemon process has exited;
3. start an embedded app-server;
4. resume the same root graph;
5. reconcile durable events produced before the disconnect;
6. mark any in-flight turn, subprocess, or approval that died with the daemon as interrupted; and
7. display a concise recovery notice.

If the writer lock is still held, the TUI must not force ownership. It should enter a recoverable
connection state, retry the local daemon, and offer an explicit return-to-embedded action once
ownership becomes available.

Remote disconnection remains a reconnect flow. It must not instantiate local runtimes for remote
threads.

## Backend restoration versus visible resume

The replacement app-server must reconstruct Core session state from the rollout and state
database. This is a protocol-level resume even though the user should not experience the normal
TUI resume workflow.

The TUI must retain rather than rebuild:

- transcript cells, including command presentation;
- full-transcript and pager state;
- composer text, attachments, mentions, and queued user input;
- active selection, agent picker state, and navigation stack;
- scroll position and focus; and
- transient presentation-only notices that remain meaningful.

The backend should replay only events after the TUI's last acknowledged durable item. Replayed
items must be deduplicated by stable item identity, not text. This prevents both a blank transcript
and duplicated rows after handoff.

Keeping existing transcript cells also preserves visible `exec_command` history during automatic
fallback. The separate cold-resume gap where some command presentation cannot be reconstructed
from stored rollout items remains governed by the resume-tool-history work.

## Persistence and auditability

Handoff should persist a small transition record containing:

- transition ID;
- root thread ID;
- source and destination kinds (`embedded` or `localDaemon`);
- requested, quiesced, detached, attached, rolled-back, and failed states;
- user-requested, daemon-exit, or recovery reason; and
- timestamps and actionable failure details.

Do not persist authentication tokens, complete socket credentials, or unrelated process
environment.

The TUI should render durable presentation such as:

```text
• Session moved to local background server
• Local background server exited; session restored in this TUI
■ Could not restore locally: thread <id> still has an active writer
```

These are harness lifecycle records and should not enter model context by default.

## Memory and performance expectations

Moving several sessions to one daemon can share app-server process overhead, model and plugin
catalogs, caches, managers, and other infrastructure that would otherwise be duplicated in each
TUI process. Each loaded thread still retains its own model history, session state, agent control
plane, and environment selections, so handoff does not eliminate the dominant context data for
large conversations.

The daemon should report:

- loaded root and descendant counts;
- resident and executing agents per root;
- approximate process memory;
- handoff and fallback counts;
- recovery latency; and
- writer-lock conflicts.

These measurements should guide any later global residency or memory policy.

## Failure handling

| Failure | Required behavior |
| --- | --- |
| Daemon cannot start | Keep the embedded graph unchanged. |
| Graph is busy | Wait with user visibility or reject without changing ownership. |
| Source flush fails | Keep source ownership and report the failing thread. |
| Destination cannot acquire a writer lock | Attempt source rollback; never force the lock. |
| Destination restores only part of the graph | Tear down the partial destination and restore the complete source graph. |
| TUI disconnects after destination commits | Reconnect to the committed owner using the transition record. |
| Daemon exits during an active turn | Restore durably recorded state locally and mark incomplete work interrupted. |
| Local socket drops while daemon remains alive | Retry; do not create a competing runtime. |
| Remote socket drops | Reconnect remotely; never fall back locally. |

Transition operations should be idempotent by transition ID so retrying after an uncertain response
returns the committed owner rather than repeating detach or resume.

## API considerations

Active app-server API development belongs in V2. The implementation will likely need:

- a graph-wide quiesce and detach operation on the source;
- a graph resume operation on the destination;
- a transition status query for recovery after an uncertain response; and
- a notification when the app-server transport changes or recovery completes.

Existing `thread/resume` remains the primitive for reconstructing an individual runtime, but the
TUI should not assemble a root graph through unrelated best-effort calls. The graph transition
needs one coordinated result that identifies every resumed thread and any interrupted work.

Wire payloads should use stable thread IDs and camelCase fields. Any experimental RPC must update
the app-server documentation and generated schemas when implementation begins.

## Required coverage

1. An idle embedded root with idle descendants moves to the daemon with the same UUIDs, aliases,
   roles, nicknames, model settings, and effective model context.
2. `/agents` becomes available immediately after connection without restarting the TUI.
3. Explicit disconnect restores the same graph locally without stopping other daemon sessions.
4. Killing the daemon causes two connected TUIs with different roots to recover independently.
5. A daemon that remains alive and owns a writer lock prevents local fallback without terminating
   the TUI.
6. A restarted daemon cannot steal a graph already restored by an embedded TUI.
7. Two TUIs attempting to own the same thread receive a clear writer-conflict result.
8. Active turns and running unified-exec sessions block the initial idle-only handoff.
9. Automatic fallback retains transcript cells, command presentation, composer input, selected
   agent, and scroll position.
10. Durable items produced immediately before disconnect are rendered exactly once after event
    reconciliation.
11. Queued agent turns and response observations either transfer completely or cause the handoff
    to roll back.
12. Per-root V1 and V2 limits remain unchanged after handoff, while separate roots in one daemon
    retain independent capacity.
13. Remote app-server disconnection never starts a local runtime.
14. Transition retries after lost responses return the already committed owner and do not detach
    twice.

## Rollout

1. Add read-only `/server` status and connection diagnostics.
2. Implement idle embedded-to-daemon handoff for a root with no descendants.
3. Preserve the TUI transcript while rebinding its app-server session.
4. Extend handoff to an idle complete root graph.
5. Add explicit daemon-to-embedded disconnect.
6. Add automatic fallback after confirmed local-daemon process exit.
7. Measure real memory savings and recovery latency before considering global daemon resource
   limits or busy-graph draining.
