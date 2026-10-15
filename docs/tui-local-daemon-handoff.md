# TUI local app-server handoff and fallback

Status: cold local-daemon recovery implemented, including restoration of the owning agent tree and continuation of eligible unfinished turns; executable validation is outstanding. Healthy graph handoff remains a deferred proposal. The `/server` commands and graph transition records below are not implemented.

Related work:

- [TUI user-controlled multi-agent dispatch](tui-agent-control.md)
- [TUI resume tool history](tui-resume-tool-history.md)
- [TUI alternate-screen behavior](tui-alternate-screen.md)

## Summary

Codex TUI can run against an embedded app-server or a shared local app-server daemon. The daemon-backed mode enables the daemon-wide `/agents` dashboard and can reduce duplicated app-server infrastructure across multiple TUI processes. Today, however, the app-server target is selected at startup:

- `/agents` is unavailable to an embedded session;
- starting the local daemon from that dialog does not move the current session to it; and
- non-embedded disconnections enter the reconnect flow, which can now restore the selected local conversation's owning agent tree in an embedded app-server after proving that its connected daemon exited.

The remaining proposal is explicit, reversible handoff between the embedded app-server and the local daemon. Implemented cold recovery reconstructs durable conversation state through ordinary resume, preserves editable local input, and automatically continues eligible unfinished turns without asking Main to call `resume_agent` or send continuation prompts. It does not recreate process-local delivery capabilities, approvals, or tool handles.

This is a local-host feature. A remote app-server owns a potentially different workspace, environment, credential, and permission boundary and must not silently fall back to the local machine.

## Implemented cold recovery

The TUI retains a native process monitor from its actual local socket connection before the WebSocket and initialization handshakes complete. Linux uses a pidfd, macOS uses a process-exit kqueue event, and Windows retains a process handle. A lost socket, missing PID file, refused connection, or unavailable process monitor is not evidence of process death. Missing or denied native monitoring leaves ordinary reconnect behavior unchanged.

Only an attached `LocalDaemon` Unix-socket session with a known displayed thread ID can fall back. Explicit `--remote` connections, including Unix sockets and loopback WebSockets, never fall back. An overview without a selected conversation and an initial submission whose thread ID was never received cannot be reconstructed automatically. Requiring the daemon at startup does not prevent recovery after a later verified death.

Recovery reuses launch-time home, auth selection, configuration loader, executor manager, and network-policy owners; starts the same embedded app-server used by `--no-daemon`; and retains thread UUIDs. The old daemon was the writer, and its exit releases its operating-system locks. Ordinary canonical-history, parent ownership, and writer-lock checks still apply; recovery neither deletes locks nor bypasses these checks.

Recovery has two phases. Preparation walks from the selected thread to its owning root, restores open descendants parent-first, and attaches them to the new embedded server without waking idle work. Closed or transferred agents are not reopened. Restoration retains the latest thread-owned reasoning-summary preference, including child settings absent from the launch configuration; it does not replace newer settings with an unfinished turn's older selection. Failure to restore the selected thread or an owning ancestor leaves the TUI offline with its draft intact; unavailable descendant branches are reported individually. One preparation attempt is allowed per embedded server instance. A server-bound, single-use plan authorizes continuation only after the TUI adopts the new connection and event stream.

Continuation considers each restored thread's latest durable, unfinished regular turn. It validates receiver-owned canonical history and matching saved permissions, local environment, model, collaboration mode, and plugin selection, then uses the existing recovery submission with the original turn ID and no new user message. Newly persisted turn contexts include a recovery snapshot for the output schema, resolved service tier, parent lineage, and configured summary preference; an absent schema within that snapshot certifies that the original turn had no schema. The guarded admission also compares resolved settings before sampling. Completed, explicitly interrupted, errored, and idle agents remain idle. Newer or already active work invalidates an old candidate. Changed settings, ambiguous ancestry, unavailable context or recovery snapshots, non-regular work, and unsupported environment selections leave that agent paused with an explanation. Older rollouts remain readable and explicitly resumable, but their missing start-only constraints cannot be inferred for automatic continuation.

Local drafts and paused input survive. Exact durable client-message receipts can confirm previous admission; uncertain submissions and retained local queues are not automatically resent. Stale requests, task-tool connections, interactive approvals, and response routes are retired. Recovered background threads are live in the new server; cached threads outside the owning tree remain replay-only. Restoring a turn restarts sampling from saved context, not the former model stream or subprocess. Canonical function, custom-tool, and shell calls without matching durable outcomes leave their agent paused for explicit reconciliation, before prompt normalization can replace missing results with `aborted`. Recovery does not directly re-execute historical tool calls or promise to restore unflushed output. External commands may outlive the daemon, and their effects still require reconciliation; this is not an exactly-once execution guarantee.

Tool dispatch also checks the canonical publication result before creating its execution future. A rejected append or lost append/flush acknowledgment quarantines publication and stops dispatch, so a failed write cannot produce an external tool effect with no recorded call. Fault-injected coverage checks both failures against an acknowledged-call control.

The recovery notice identifies the new embedded owner. Daemon-wide `/agents` is no longer available. Visible history is rebuilt through the existing bounded resume/replay path; exact scroll position, overlays, and every transient transcript cell are not preserved by this implementation.

Regression coverage includes native peer lifetime, local versus TCP client monitoring, embedded recovery with saved history and editable input, a fresh model turn after recovery, writer-lock enforcement, stale-event isolation, and remote/live-peer/unknown-thread exclusions. A public session-facade fixture kills a test-owned subprocess after a child's input and turn context are on disk, then checks open-tree restoration from child selection, original turn identity, preserved output schema, service tier, and reasoning summary, no added continuation prompt, and refusal after a mid-turn collaboration-mode or summary change or a durable sleep-tool call without its outcome. Selector and admission coverage checks inherited-history boundaries, terminal turns, newer-work guards, and matching call/output identities and families. These tests require executable CI validation; static inspection and formatting do not establish that they pass.

Remaining lifecycle qualification: exercise an actual killed daemon through the full TUI event loop and cancellation during embedded startup/resume. The fallback fixture composes peer death with separately shut-down embedded ownership; the tree fixture kills its real in-process server owner but invokes recovery through the public session facade rather than the terminal event loop. Legacy and V2 tree restoration, transferred/partially unavailable branches, and cross-server plan rejection need broader integration coverage. Run the native-monitor and recovery coverage on Linux, macOS, and Windows before treating cross-platform recovery as qualified. The new protocol metadata also requires generated-schema validation in remote CI.

## Remaining handoff goals

- Let an existing embedded TUI move its current root thread graph to the shared local daemon.
- Let a daemon-backed TUI move its graph back to an embedded app-server.
- Extend cold daemon-death recovery with faithful retention of transient interface state.
- Keep thread UUIDs, validated durable graph metadata, aliases, roles, nicknames, and canonical model context. Preserve queued work only through an explicit authenticated healthy-handoff protocol, not by inferring it from audit history after process death.
- Preserve the TUI's rendered transcript, composer, focus, scroll position, selected agent, and overlays rather than rebuilding the visible interface through the ordinary cold-resume path.
- Use real per-thread writer exclusion together with root ownership and admission checks. A backend without the required exclusion capability must reject the transition rather than treating unsupported locking as success.
- Keep agent concurrency accounting scoped to each root graph after handoff.
- Make every handoff, fallback, conflict, and interrupted turn visible and auditable.
- Allow `/agents` to operate after a successful connection without forcing the user to restart Codex.

## Non-goals

- Seamlessly moving an actively sampling model turn or a running subprocess between processes.
- Falling back locally from a remote app-server connection.
- Replacing the per-thread writer lock with a second ownership or lease system.
- Treating every stored rollout as loaded in the daemon-wide `/agents` dashboard.
- Adding a daemon-wide aggregate agent limit as part of handoff.
- Reconstructing process-local queues, scoped `m` grants, pending observers, or accepted delivery capabilities from durable audit history after a cold boundary.
- Fixing all cold-resume transcript reconstruction gaps. A live TUI handoff should avoid invoking visible transcript reconstruction, but ordinary `codex resume` remains separate work.

## Current ownership and limits

The local daemon is a shared process, but multi-agent admission is not daemon-global.

Each root's local control plane is implemented by `LocalAgentControl`, with operations exposed through the `AgentControl` abstraction. Its registry, response-observation state, V2 residency, and V2 execution limiter are shared by its owned descendants. A different root loaded by the same daemon has its own control plane and configured limit. Canonical UUIDs and durable parentage identify threads; they do not replace live control ownership, writer exclusion, or exact-instance admission.

Consequently:

- V1 counts live descendant agents within one root control plane.
- V2 reserves capacity for the root and bounds resident or executing descendants within that root control plane.
- moving a graph between embedded and daemon-backed app-servers must not reset its accounting or combine it with another root's accounting;
- a daemon containing many roots can exceed any one root's configured agent count in aggregate.

A future global daemon resource budget may be useful, but it must be an independently named and reported limit rather than silently changing `agents.max_concurrent_threads_per_session`.

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

`/server connect local` starts the configured local daemon when it is absent, then moves the current root graph to it. `/server disconnect` moves a local-daemon graph back into the current TUI process. It is unavailable for remote app-server sessions.

The embedded `/agents` unavailable dialog should also offer:

1. `Move this session to shared server`
2. `Start background server only`
3. `Return to this session`

Starting the server alone retains its current behavior and does not move the session.

The exact command name may be reconciled with any general connection-management surface that lands before implementation. The required product distinction is between starting a daemon and transferring the current session to it.

## Handoff boundary

A root session and its owned descendants move as one graph. Moving only the currently displayed thread would split one root's `LocalAgentControl`, its aliases, response observations, and queued turns across two app-server processes.

The first implementation should admit a handoff only when:

- no thread in the root graph has an active model turn;
- no graph-owned unified-exec process is running;
- no lifecycle mutation or queued-turn admission is in progress;
- every accepted canonical publication and exact-instance delivery obligation has settled without ambiguity; and
- every rollout and durable graph update has an acknowledged flush. This is not an fsync guarantee.

The existing queue is process-local, and cold history does not restore its pending entries or scoped grants. Healthy transfer therefore needs a new authenticated, versioned transfer protocol with explicit root authorization, source fencing, destination admission, and atomic disposition of each transferable pending entry. The first implementation should reject graphs containing state that this protocol cannot transfer. Already accepted receipts remain attached to their exact runtime instances and must settle there; the destination must never retarget them or manufacture acknowledgement from readable history. An active `m` grant cannot outlive its target turn merely because a graph is being moved.

The TUI can wait for ordinary work to finish or ask the user to interrupt it. It must not silently interrupt active work merely to optimize memory.

Later work may support draining a busy graph before handoff, but migrating active model streams, subprocess pipes, or approval requests is unnecessary.

## Embedded to local-daemon flow

```text
user: /server connect local
        |
        +-- start or probe local daemon
        +-- resolve the current root and complete owned graph
        +-- reject or wait while the graph is busy
        +-- freeze new graph admission
        +-- settle accepted obligations and flush canonical durable state
        +-- capture supported pending state in an authenticated transfer manifest
        +-- detach the graph from the embedded runtime
        +-- release its per-thread writer locks
        +-- ask the daemon to admit the authorized graph transition
        +-- daemon acquires every writer lock and restores graph ownership
        +-- rebind TUI app-server requests and notifications
        +-- retain all existing TUI presentation state
        +-- unfreeze admission
```

The destination must validate complete root ownership and every descendant using the authenticated transition plus canonical metadata, then acquire all required writer exclusion. Durable parentage and aliases alone are not transfer authority. A healthy handoff is distinct from ordinary cold resume or fork: only pending state explicitly supported by the new transfer contract may be re-established, with fresh destination capabilities. That contract is future work, not a property of today's `thread/resume`.

If destination admission is positively rejected before commitment, the protocol may authorize source recovery after proving the destination cannot still publish and reacquiring all writer locks. An uncertain response requires status reconciliation, not a second detach/resume attempt. A competing-writer error must identify the owned thread. Failure cleanup must never delete an existing rollout merely because its replacement runtime was newly created.

## Local-daemon to embedded flow

Explicit disconnect uses the same graph-wide protocol in reverse:

1. verify that the graph is idle;
2. freeze graph admission in the daemon;
3. settle exact-instance obligations, flush durable state, and capture supported pending state through the authenticated transfer protocol;
4. detach the graph and release writer locks;
5. start an embedded app-server;
6. admit the authorized transition and acquire all graph writer locks;
7. rebind the TUI; and
8. retain the existing interface state.

Disconnecting one graph must not stop the daemon or affect other connected TUI sessions.

## Automatic fallback after daemon loss

For a supported local store, operating-system-backed writer exclusion is necessary to prevent competing rollout writers, but does not alone authorize graph adoption or replay:

- if the daemon process died, the operating system releases its locks and embedded restoration can be considered after validating root authority and canonical recovery state;
- if the daemon is merely unreachable but still alive, its writer locks remain held and embedded restoration receives an ownership conflict;
- a restarted daemon cannot reload a thread already restored by an embedded TUI because that TUI owns the same lock.

The implemented cold path above restores the owning tree, reconciles durable conversation state, and resumes eligible unfinished turns after adopting the replacement connection. Retaining every transient interface cell and exact scroll position remains future work. Process-local approvals, queue entries, observation routes, and subprocess handles are not transferred.

Daemon death is not a healthy transfer. Unadmitted queue entries, scoped reply grants, pending automatic observations, and unresolved accepted capabilities cannot be reconstructed from audit rows, draft text, or a missing RPC response. Cold history remains audit-only for those capabilities. Automatic continuation is restricted to already admitted, durably recorded unfinished work under the recovery contract above; it does not authorize replaying ambiguous submissions. The TUI preserves an unsubmitted draft but never automatically resubmits a possibly accepted prompt. Ambiguous candidates remain paused.

If the writer lock is still held, the TUI must not force ownership. The current cold path fails closed and retains local input. A future explicit return-to-embedded action could retry after ownership becomes available.

Remote disconnection remains a reconnect flow. It must not instantiate local runtimes for remote threads.

## Backend restoration versus visible resume

The replacement app-server must reconstruct canonical conversation state from the rollout and state database while separately validating the healthy-transfer or cold-recovery authorization. This is backend reconstruction even when the visible TUI is retained; it does not preserve the old runtime's capabilities by itself.

The TUI must retain rather than rebuild:

- transcript cells, including command presentation;
- full-transcript and pager state;
- composer text, attachments, mentions, and unsubmitted drafts with their resource authority; accepted or ambiguously submitted inputs remain reconciliation state, not automatic new submissions;
- active selection, agent picker state, and navigation stack;
- scroll position and focus; and
- transient presentation-only notices that remain meaningful.

The future protocol needs a canonical reconciliation cursor and bounded replay contract. Stable item identity, source lineage, and canonical provenance should prevent duplicate presentation; matching text or an ID-shaped payload is not sufficient. A UI acknowledgement is not proof that a backend write or delivery receipt succeeded, and this proposal does not claim exactly-once recovery across unknown outcomes.

Keeping existing transcript cells also preserves visible `exec_command` history during automatic fallback. The separate cold-resume gap where some command presentation cannot be reconstructed from stored rollout items remains governed by the resume-tool-history work.

## Persistence and auditability

Handoff should persist a small transition record containing:

- transition ID;
- root thread ID;
- source and destination kinds (`embedded` or `localDaemon`);
- requested, quiesced, detached, attached, rolled-back, and failed states;
- user-requested, daemon-exit, or recovery reason; and
- timestamps and actionable failure details.

Do not persist authentication tokens, complete socket credentials, or unrelated process environment.

The TUI should render durable presentation such as:

```text
• Session moved to local background server
• Local background server exited; session restored in this TUI
■ Could not restore locally: thread <id> still has an active writer
```

These are harness lifecycle records and should not enter model context by default.

## Memory and performance expectations

Moving several sessions to one daemon can share app-server process overhead, model and plugin catalogs, caches, managers, and other infrastructure that would otherwise be duplicated in each TUI process. Each loaded thread still retains its model history, session state, and environment selections, while each root retains its control plane. Handoff does not eliminate the dominant context data for large conversations.

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
| Source flush fails or loses acknowledgement | Keep ownership fenced, retain accepted obligations, quarantine ambiguous publication, and report the failing thread; do not certify success by readback or retry. |
| Destination cannot acquire a writer lock | Never force the lock; recover the source only after positively excluding destination commitment and reacquiring complete ownership. |
| Destination restores only part of the graph | Fence partial destination runtimes and reconcile transition status before recovery; retain all existing canonical rollouts and receipts. |
| TUI disconnects after destination commits | Reconnect to the committed owner using the transition record. |
| Daemon exits during an active turn | Perform authorized cold recovery of durable state, mark incomplete work interrupted, and leave lost queues, grants, and unresolved capabilities inert. |
| Local socket drops while daemon remains alive | Retry; do not create a competing runtime. |
| Remote socket drops | Reconnect remotely; never fall back locally. |

Transition operations require a durable status protocol keyed by transition ID. After an uncertain response, query and reconcile that status rather than blindly repeating detach or resume. If ownership cannot be proved, retain a visible unresolved state.

## API considerations

Active app-server API development belongs in V2. The implementation will likely need:

- a graph-wide quiesce and detach operation on the source;
- a graph resume operation on the destination;
- a transition status query for recovery after an uncertain response; and
- a notification when the app-server transport changes or recovery completes.

Existing `thread/resume` remains a primitive for reconstructing an individual runtime, not a graph-transfer or receipt-recovery API. The TUI must not assemble a transfer through unrelated best-effort calls. The new graph transition needs one coordinated result identifying every admitted thread, root ownership, supported transferred pending state, and interrupted or unrecoverable work.

Wire payloads should use stable thread IDs and camelCase fields. Any experimental RPC must update the app-server documentation and generated schemas when implementation begins.

## Remaining graph-handoff coverage

1. An idle embedded root with idle descendants moves to the daemon with the same UUIDs, aliases, roles, nicknames, model settings, and effective model context.
2. `/agents` becomes available immediately after connection without restarting the TUI.
3. Explicit disconnect restores the same graph locally without stopping other daemon sessions.
4. Killing the daemon causes two connected TUIs with different roots to recover independently.
5. A daemon that remains alive and owns a writer lock prevents local fallback without terminating the TUI.
6. A restarted daemon cannot steal a graph already restored by an embedded TUI.
7. Two TUIs attempting to own the same thread receive a clear writer-conflict result.
8. Active turns and running unified-exec sessions block the initial idle-only handoff.
9. Automatic fallback retains transcript cells, command presentation, composer input, selected agent, and scroll position.
10. Canonical items produced immediately before disconnect reconcile without duplicate presentation, including lost write acknowledgements and conflicting provenance.
11. Supported pending queue/observation state transfers only through the authenticated healthy protocol; unsupported state blocks handoff, and daemon death never recreates it from audit history.
12. Per-root V1 and V2 limits remain unchanged after handoff, while separate roots in one daemon retain independent capacity.
13. Remote app-server disconnection never starts a local runtime.
14. Transition-status reconciliation after lost responses identifies the committed owner or remains visibly unresolved; it never detaches twice.
15. Unsupported writer exclusion, partial destination startup, and ambiguous source publication fail closed without deleting rollouts or retargeting accepted receipts.

## Rollout

1. Add read-only `/server` status and connection diagnostics.
2. Implement idle embedded-to-daemon handoff for a root with no descendants.
3. Preserve the TUI transcript while rebinding its app-server session.
4. Extend handoff to an idle complete root graph.
5. Add explicit daemon-to-embedded disconnect.
6. Extend the implemented conversation-level fallback to the validated graph-wide handoff contract.
7. Measure real memory savings and recovery latency before considering global daemon resource limits or busy-graph draining.
