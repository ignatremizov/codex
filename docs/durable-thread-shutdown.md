# Durable runtime shutdown

`CodexThread::shutdown_durably_and_wait()` stops one runtime and acknowledges that its final
history and pending metadata have been persisted and its store has released the writer lease.
It does not unload the thread from a manager, stop descendants, archive history, or close durable
agent aliases. A caller coordinating ownership handoff must fence subtree membership and unload
the exact stopped runtime instances separately.

The internal shutdown operation uses the existing submission-admission boundary. It closes new
ordinary input and completion-delivery admission, then waits for previously accepted completion
deliveries before enqueueing shutdown. It does not hold the submission send lock while waiting
for those deliveries.

Once accepted, the submission loop owns shutdown independently of the requesting caller.
Execution admission closes before the active task is stopped. The session's execution manager
retains exact process handles independently of its resumable-terminal inventory, including
processes rejected during sandbox classification. It confirms local exit acknowledgements or
authoritative executor exit reports; a kill request, cancellation token, or synthetic exited
status is not enough. Failed confirmation retains ownership for retry.

The manager also drains admitted launch tasks, output delivery, exit watchers, and user-shell
commands. Producer registration precedes publication, and a failed or canceled producer cannot
be mistaken for successful completion. Drain waits do not hold process-store or interaction
locks. A ten-second process-drain budget returns an error without closing the writer, while
accepted work retains responsibility for cleanup. Subprocess capture reaps the child and joins
its output readers before returning a terminal result.

Code-mode shutdown and execution drain settle together, followed by another execution drain
for work admitted by callbacks during code-mode closure. Current, retired, and not-yet-published
guardian runtimes are retained behind the same guardian-creation fence. Shutdown waits for owned
creation tasks and for each exact child actor's durable acknowledgement without holding the
guardian selection lock. Failed child shutdown retains ownership for retry.

Delegate facades retain the underlying actor's shutdown endpoint independently of their event
and input forwarding tasks. Guardian cancellation uses that endpoint; facade closure or event
channel termination is not a successful shutdown acknowledgement. This guarantees completion
of actor-admitted work, not preservation of inputs still queued in a forwarding facade.

Runtime services then stop. Accepted completion context is persisted, thread-stop
lifecycle hooks run, and pending metadata is completed before closing persistence. Normal
interruption semantics apply: this operation does not preserve an active turn as an unfinished
turn for recovery.

A persistence error is returned to the caller without publishing `ShutdownComplete` or
terminating the submission loop. Completed teardown phases are retained for retry, and ordinary
work admission stays closed. Retrying the operation resumes the failed phase. A legacy shutdown
request received during this retry state also follows the durable path instead of bypassing its
persistence barrier.

Successful acknowledgement is retained after loop termination, so concurrent or later callers
can observe the same result. Unexpected termination and legacy shutdown alone do not establish
durable success. The rollout writer separately retains its own successful stop acknowledgement;
that acknowledgement permits cleanup retries but is not itself evidence of writer-lease release.

## Explicit unload and TUI exit

App-server `thread/unload` resolves the owning spawn root even when the selected member is a
child or the root itself is already unloaded. It captures loaded descendants behind the existing
parent-before-child lifecycle locks, checks that no other connection subscribes to the captured
subtree, and fences new subscriptions. It does not treat ordinary fork ancestry as ownership.

After preflight, captured runtimes are sealed against ordinary input, spawn, reopening, close,
eviction, and removal. The lifecycle locks are then released before draining accepted completion
deliveries: those deliveries may themselves need a child's lifecycle lock. The runtime seal stays
set across failed attempts; retries reacquire lifecycle locks only to finalize exact-instance
removal. If an ancestor is already unloaded, its sealed loaded descendants still prevent cold
resume or an agent-controlled resume from recreating that ancestor during the drain.

An admitted spawn owns its parent and child lifecycle boundaries through initialization,
publication, and rollback, even if its response waiter disappears. Cancelled setup suppresses
terminal presentation before shutdown. If durable shutdown or closing the cancelled spawn's
alias fails, the existing loaded-runtime inventory retains that exact sealed instance and its
remaining alias rollback obligation. Unload retries finish both obligations before removal.
This closes only cancelled, never-acknowledged spawn identities, not ordinary unloaded aliases.

The accepted operation outlives its response waiter. Every captured runtime must acknowledge
durable shutdown before any captured entry is removed. A failure retains every entry for retry,
including already-stopped actors. Successful cleanup removes only the captured instances and
their process-local residency; history and durable aliases remain available. Retrying a known
already-unloaded subtree returns its root and an empty list of unloaded runtimes.

TUI `/quit`, `/exit`, and keyboard paths that actually exit use this operation for the owning
root, regardless of the viewed agent. Interrupt-first keyboard handling is unchanged. An unload
error keeps the UI open, restores the composer, and explains how to retry.

`/disconnect` explicitly detaches from a shared app server and leaves server-owned work running.
It is refused for an embedded server: exiting the TUI also ends its owned server and cannot
preserve running work. Navigation continues to unsubscribe without unloading, and unexpected
connection loss retains the existing unsubscribed idle grace period.
