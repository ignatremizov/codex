# Code-mode durable shutdown

Code-mode has separate fast and durable shutdown operations. Fast shutdown preserves
the existing interruption behavior: it closes cells without waiting for arbitrary
delegate cleanup. Durable shutdown is a prerequisite for releasing a thread's writer
ownership during a graceful quit or authentication handoff.

The thread-owned service fences new nested dispatch, rejects queued notifications and
tool calls, and cancels accepted execution. Accepted notifications and nested tool
tasks retain their tracking tokens until they end, even when their original waiter
disappears. Provider closure and accepted dispatch drain run concurrently. A successful
provider close alone is not sufficient to release the writer.

Shutdown attempts belong to the service, not to the requesting caller. A caller's
deadline does not abort accepted work. A subsequent request joins an in-progress
attempt or retries a failed phase. Confirmed provider closure is retained across
retries. A callback panic is reported as a failed drain rather than being mistaken for
an empty, successfully drained task set.

## Provider initialization

Built-in providers expose a logical session owner before starting backend work through
`create_owned_session`. Their existing eager `create_session` API remains available.
The service retains accepted factory work independently of callers and publishes its
session before closing it.

The gRPC logical session owns each accepted opening attempt before its first RPC.
After the remote session ID arrives, it retains that exact generation before awaiting
the tool subscription. A subscription failure or dropped execution caller therefore
does not discard the cleanup owner. Durable shutdown joins accepted opening work and
does not admit another generation after its fence.

An opening RPC that fails before returning an identifiable session remains unresolved.
A later execution request cannot replace that failure with a successful generation.
Failures before any opening RPC, or after a cleanup handle has been registered, remain
distinguishable from an unidentified remote session.

An unused service does not create a provider merely to shut down. A failed custom
factory with no returned cleanup handle is not equivalent to an unused service:
durable shutdown reports the initialization error and remains fail-closed. The
compatibility default for `create_owned_session` cannot prove that an unsuccessful
custom factory left no backend resources, so the service does not retry that factory
and overwrite its unresolved failure.

## Closure proof

For the process provider, durable shutdown waits for each current or retired session's
local closure and accepted callbacks. A successful session-close response proves
backend closure. A broken connection does not: the process supervisor must observe
the child exit. Failed OS waits retain supervisor ownership until a subsequent wait
confirms that exit. Explicitly rejected local session creation is safe because the
process host inserts a newly allocated session only on a successful open operation.

For gRPC, durable shutdown requires an acknowledged `CloseSession` request. A broken
transport or locally closed stream is not remote-exit proof. Failed closes retain the
original session ID and transport for retry, including failed opening generations.
After stream admission has stopped, durable shutdown also waits for accepted callback
tasks. Fast shutdown does not wait on that callback tracker.

The enclosing thread shutdown applies the bounded wait and drains owned processes
concurrently, since accepted callbacks can depend on process termination. A timeout
keeps the writer fenced and the cleanup attempt owned; it does not establish that
shutdown completed.
