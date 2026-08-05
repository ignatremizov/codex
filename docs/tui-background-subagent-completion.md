# TUI background subagent completion rendering

When a v1 or v2 child finishes without an active `wait_agent`, its result now
appears immediately in the parent transcript as `Agent finished`. Model-context
delivery is unchanged: v1 injects `<subagent_notification>`, while v2 queues an
inter-agent message. V1 records that notification through a detached durable
context commit even while a parent turn is active, so interrupting the turn
cannot discard the child result.

After context delivery succeeds, core emits a canonical parent `ItemCompleted`
agent message through the exact parent thread instance that accepted the context
submission. V2 also flushes the reserved completion context before exposing
that lifecycle while retaining the queued mail for active or later
`wait_agent` handling; the normal turn-input commit reuses the same durable
item. The item combines core-authored completion metadata, a reserved UUIDv7
status ID, a canonical agent-reference/payload envelope, and reserved
`Commentary` identity. Idle parents persist it in a completed history-only turn
with a UUIDv7 turn ID; both history modes retain it for replay without colliding
after resume. The complete rollout batch is flushed before the item lifecycle
(`ItemStarted`, then `ItemCompleted`) is emitted live. A failed append therefore
cannot create a live-only row; a detached worker retains and retries the same
canonical item while that parent Session remains active. Shutdown waits for an
accepted retry, while rollback quarantine stops a permanently rejected v1
notification retry so the old Session can terminate without deadlocking.
Because a store can report a flush failure after committing the append, retries
first reconcile the stable completion identity against canonical history. That
identity lookup scans across semantic-compaction boundaries even when ordinary
paginated model-context loading returns only the latest bounded suffix.
Canonical presentation reuses its original history-only turn ID and repairs
only a missing batch suffix; v1 and v2 model-context delivery reuse their
reserved response-item IDs. Cold reconstruction also collapses repeated
reserved completion-context IDs from rollouts written by older retry behavior.

This is the provenance boundary: ordinary provider output cannot set the typed
completion metadata, reserved-looking provider IDs are moved out of the
completion namespace, and a completion-context `msg_x_<uuid>` identity survives
input normalization only when core has registered a matching one-shot
authorization for that exact destination Session instance. The `msg` prefix
remains provider-compatible when V1 completion context is serialized as a
user-role message. Submission failure or cancellation rolls the authorization
back. Manager removal does not revoke completion work
the removed Session already accepted; final Session teardown clears only
capabilities owned by that generation. Delayed shutdown, failed submission, and
residency cleanup remove a manager entry only when it is still the exact
retained thread instance; registry and presentation cleanup run under that
same check, so a resumed Session with the same ThreadId is not removed or
cleared. App-server idle unload and rollback quarantine teardown also retain
the concrete thread, serialize against thread lifecycle mutations, and clear
app-server state only after removing that exact instance.
Completion submission waits through a temporary rollback reservation and
revalidates that exact parent Session before retrying. Reload quarantine and
shutdown remain terminal for the old Session generation.
Accepted completion context and canonical completion items are out-of-band
rollout artifacts. Exact rollback preserves them even when their durable records
fall inside the raw range of the rolled-back user turn. Completion context uses
a reserved response-item identity paired with core-authored delivery metadata
in both v1 and v2, so this preservation never depends on parsing user-authored
text that resembles a completion envelope. When a durably completed
`wait_agent` owns presentation and suppresses the canonical background row,
exact rollback preserves that terminal wait item as the visible completion
artifact instead.
Paginated projection rebuilds from the canonical exact-rollback mask when such
a marker arrives, so its SQLite view retains the same trusted completion
artifacts and excludes rolled-back lookalikes. Paginated turn summaries include
those retained background-completion and terminal-wait rows.
V2 completion context is still recorded for the parent model under that
reserved identity, but app-server omits the context-only item from the
transcript so the canonical completion item is the sole visible row. JSONL and
human `codex exec` output likewise exclude the canonical background row from
main-agent final-answer selection.

Completed/errored v2 turn lifecycle uses direct Session delivery; the watcher
owns standalone raw errors and shutdown/not-found fallbacks. Terminal
publication is serialized with status publication and is armed only after
successful Session initialization. A final status therefore closes one child
run, remains authoritative over later teardown or completion events, and is
rearmed only by a later running status. Error-bearing `TurnComplete` events
remain errored. Removal publishes `NotFound` before retained thread references
can delay `Session` drop. Reloaded v2 agents attach a new watcher, and watcher
cleanup is scoped to the terminal turn it processed so an older watcher cannot
erase a newer generation. Explicit child closure clears its generation marker;
watcher registration is deduplicated per child Session instance. Fresh spawns
attach that watcher before exposing the child or submitting its initial input,
so an early submission failure still drains and forwards the terminal fallback.
Operational
v2 residency unloads disarm completion presentation before shutting down, so
cache eviction is not reported as a logical child result. Each watcher captures
its originating thread trace context without retaining the Session, so an old
watcher cannot write its result into a reloaded Session's trace.

The TUI delegates background rows and completed waits to the same per-agent
detail renderer, preserving statuses, multiline/error formatting, preview
limits, and hidden-row markers. Each terminal transition captures an immutable
presentation token before publishing its status. An active wait owns that token
only after its completed wait item, populated with the captured terminal
statuses, reaches the parent event channel. That owning wait item is retained by
legacy and paginated history so replay does not depend on a suppressed
background row. Wait ownership commits only after the completed wait item has
also been flushed to canonical history. Persistence failure therefore releases
the token to the retrying background renderer even if the wait item reached the
live event channel. When a flush reports failure after committing, core
reconciles the wait item's stable identity and current turn against canonical
history and commits ownership instead, preventing a second background row.
Matching the turn is required because provider tool-call IDs may be reused by
later turns. Cancellation before delivery releases the token to the background
renderer; cancellation afterward cannot create a duplicate row.
Terminal recording and status publication share the wait-registration lock, so
a wait cannot start in between those two steps. Freezing the wait result
prevents later terminals from being suppressed. Wait registration and commit
ownership are scoped to the exact parent Session instance; removing a parent
revokes both active and frozen waits and rejects later registration from the
retained old Session until it drops, while another Session with the same
ThreadId remains unaffected. V2
mailbox waits inspect both turn-staged and newly queued mail in delivery order,
claim only terminal tokens whose reserved completion-context items are pending,
and deterministically use the latest queued generation for a child. An
unrelated mailbox wake therefore cannot suppress a blocked child result. A
`wait_agent` started after completion remains a separate event and renders the
result again; its exact terminal state remains available after the durable
context commit until the matching wait consumes it. Drained completion mail remains leased by the
session mailbox until its durable recording task starts. Turn aborts restore
unstarted leases, while a started recording runs detached from the cancellable
turn task and consumes its authorization only after persistence succeeds.
Persistence failure requeues the communication, preventing cancellation from
discarding model context or leaking its presentation capability.
Abort cleanup leaves a taskless active-turn transition in place until lifecycle
and pending-input cleanup finish, preventing an idle history row from
interleaving into the abort sequence. Lease restoration merges turn-staged and
mailbox-owned copies by completion identity. If an earlier durable commit
fails, every still-unstarted lease is restored in original delivery order so a
later local copy cannot overtake it.
Terminal publication reserves delivery against the exact parent Session before
publishing the child's final status. Shutdown atomically closes new completion
admission, rejects later ordinary work, and waits for every earlier reservation
to queue its context and either commit wait ownership or hand its canonical row
to the durable retry worker. Reserved v2 mailbox submissions may pass the
shutdown gate only while holding that reservation, so they queue before the
shutdown operation.

Shutdown then waits for active-turn cleanup and any in-flight completion commit,
restores unstarted leases, drains every accepted v2 completion from both the
mailbox and next-turn queue, and records that context before closing rollout
persistence. A terminal publication ordered before shutdown therefore remains
model-visible after resume instead of surviving only as a presentation row.

Coverage spans real idle-parent v1/v2 completion, provenance, persistence and
replay, live rendering, terminal status fidelity, active/cancelled/late wait
ownership, staged and repeated v2 completion mail, wait-delivery commit timing,
raw terminal errors, v1 notification retention across active-turn abort,
pre-shutdown v1/v2 terminal publication, v2 mailbox and in-flight commit
retention across shutdown, shutdown/resume model-context replay, operational
eviction, Session-generation teardown and stale-manager cleanup,
generation-bound trace recording, deduplicated reload watchers, reloaded and
repeated child runs, exact-rollback races, retrying canonical persistence,
forged completion provenance, public legacy and paginated app-server replay,
and later-wait duplication.
