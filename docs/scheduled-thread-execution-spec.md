# Scheduled thread execution and shared-server exec routing

Status: implemented; generated protocol bundles and remote Verify CI are required
for executable qualification. Recorded 2026-11-09.

Schedule a prompt on an existing conversation, execute it without an open TUI,
and retrieve its result from either the CLI or TUI. Waiting for the scheduled
time must not require a sleeping shell, an active model turn, or a loaded thread.

This change also routes immediate `codex exec resume <thread> <prompt>` to
reuse an existing app-server owner, or load the thread in the app-server selected
by the CLI's auth. Treat this as delayed ordinary text input through the existing
queue, with headless result observation.

## Existing foundations

- `codex queue --steer` durably accepts a prompt. A loaded owner steers an active
  turn or starts a turn when idle. An interrupted or unloaded target waits for
  normal resume. Queue acceptance alone does not load a thread or return its
  eventual model result.
- Local servers with the same state directory discover external queue changes,
  normally within ten seconds. The loaded runtime uses its own credentials.
- The previous `codex exec resume` path starts an embedded app-server, resumes the
  conversation, and starts a turn. It can stream JSONL with `--json` and write
  the final response with `--output-last-message` / `-o`.
- Cross-process writer exclusion prevents two servers from independently writing
  the same thread. A held lock proves contention, not the endpoint or auth of its
  owner; a lock filename's existence is not proof that the lock is held.
- Unsubscribed app-server threads default to a 1,800-second idle unload delay,
  configured by `thread_unload_delay_secs`. Eligibility requires both no client
  subscribers and inactivity. Active work and agent ownership/residency checks
  remain separate safeguards.

Source anchors:

- [Queue behavior](session-message-queue.md) and
  [queue CLI](../codex-rs/cli/src/queue_cmd.rs).
- [Headless execution](../codex-rs/exec/src/lib.rs) and
  [exec arguments](../codex-rs/exec/src/cli.rs).
- [Writer exclusion](../codex-rs/rollout/src/writer_lock.rs).
- [Idle unloading](../codex-rs/app-server/src/request_processors/thread_lifecycle.rs)
  and [configuration defaults](../codex-rs/core/src/config/mod.rs).
- [Explicit unload](durable-thread-shutdown.md) and
  [agent restoration](agent-restoration.md).

## Required behavior

1. Creating a schedule authorizes loading or resuming its recorded conversation
   when the prompt becomes due. A connected TUI is optional.
2. Reuse a thread already loaded in an app-server, including a different local
   account profile sharing the same Codex home. Preserve that owner's auth and
   execution environment.
3. If no server owns the thread, load it in the app-server selected by the
   invoking CLI's auth, or the TUI's current connection when scheduling there.
4. Save one pending input record. Use normal turn history and status for results;
   do not build a separate job execution state machine.
5. Release execution and observation state after completion. An independent,
   inactive thread without subscribers returns to the normal idle unload policy.
6. App-server death and explicit thread unload continue to stop owned commands.
   Scheduling does not make shell processes survive their owning server.

The first version supports one-shot text prompts on persistent existing threads.
Recurring schedules, host wake, and arbitrary shell cron jobs can be added later.
Ephemeral, missing, and archived targets produce ordinary validation errors.
Never create a replacement conversation or implicitly unarchive one.

## User surfaces

TUI syntax:

```text
/schedule 17:30 prepare the BAR daily report
/schedule in 2h check the deployment results
/schedule list
/schedule cancel <schedule-id>
/schedule result <schedule-id>
```

The TUI uses the displayed thread. Confirm the schedule ID, resolved date/time,
timezone, and fallback auth profile. Reuse the normal queue preview for pending
scheduled input, including its due time; do not add a second task dashboard.

Equivalent CLI:

```sh
codex schedule --thread <thread-id> \
  --at '2026-11-10T17:30:00+02:00' \
  --message 'Prepare the BAR daily report'
codex schedule list --thread <thread-id>
codex schedule result <schedule-id> --json
codex schedule cancel <schedule-id>
```

Creating a schedule saves it and exits. An optional `--wait` observes delivery
and the receiving turn's result; closing that client does not cancel the input.
Results support `--json` and `--output-last-message` / `-o`, as in `exec`.
Input IDs are generated before creation and reused as queue and client-message
identities.

Immediate execution retains:

```sh
codex exec resume <thread-id> 'Prepare the BAR daily report' \
  --json -o result.txt
```

It uses the same routing and result observation without opening a TUI.

Resolve an exact session name to a UUID once, at creation. Reject ambiguous names;
renaming a conversation later must not retarget a schedule.

## Time and availability

Resolve a due UTC instant once. Confirm the full date and timezone; a bare time
such as `17:30` means the next occurrence in the submitting client's timezone.
Reject ambiguous/nonexistent local times unless an offset resolves them.
Explicit past timestamps are immediately due. A future prompt must not be
delivered early or block ordinary input already eligible for delivery.

Reuse the queue watcher's existing polling cadence, normally ten seconds.
Check due times even when the database revision has not changed: passage of time
does not itself write a queue revision. Recheck against wall-clock time after
clock changes and host sleep. Precision beyond the polling interval is not a
goal; a steer is processed at the next safe input boundary. All queue dispatch
paths must respect the due time, including idle starts and manual queue starts.

Both shared and embedded app-servers already watch the shared queue for their
loaded threads. Extend that path with due-time filtering. Creating a schedule
through an embedded TUI writes to the same database; no special handoff to a
managed scheduler is required.

At least one eligible app-server must be alive to dispatch. If the target is
loaded, that owner can handle it. If it is unloaded, a live server matching the
saved fallback auth can resume it. If neither exists, the pending row waits
until one becomes available, then delivers overdue input using normal queue
behavior. Scheduling does not boot the host or restart stopped services.
Explicit remote connections use that server's queue and environment only.
Transient one-shot exec servers consume their loaded-thread queues, but do not
claim unrelated unloaded schedules: their primary command can end at any time.
Managed servers and embedded TUI/session-command servers perform cold loading.

## Routing and authentication

Write the input to the shared queue, retaining its due time and fallback profile.
A server that already owns the thread consumes eligible input exactly as it does
for `codex queue --steer`, regardless of which profile submitted it.

For a due entry targeting an unloaded thread, the selected profile's server
attempts ordinary resume. This requires querying due entries for that profile
even when their UUIDs are absent from its loaded-thread inventory; the existing
watcher does not load them today. Existing writer exclusion and lifecycle gates
decide whether resume succeeds. A competing owner keeps the writer; leave the
same input pending for that owner. Temporary contention or unavailability retries
through normal polling. No owner endpoint registry or scheduler leader is needed.

For example, `codex-office` can submit to a thread loaded under `codex-alt`;
the loaded owner uses its existing account. If the thread is unloaded when due,
the saved office profile is the fallback. Keep profile identifiers, not tokens.
Use ordinary credential refresh and configuration/resume behavior; a schedule
does not freeze a copy of the thread's permissions or configuration.

Preserve the existing environment, approval, and agent-tree rules. A scheduled
prompt adds no permissions or dynamic-tool capabilities. Missing credentials,
archived targets, unavailable environments, or an unloaded child whose native
owner chain cannot be restored return normal errors. Do not detach children or
reinterpret remote filesystem resources on the host.

## Immediate exec and turn admission

Short-circuit prompt-bearing `codex exec resume` through the same queue/steer
path. Idle starts a turn; active steers the current turn. Unsupported active
tasks use existing queue deferral. An interrupted target needs an ordinary
resume/admission transition, which plain queue acceptance does not perform.
When unloaded, resume through the selected profile's app-server. Never silently
create a different conversation.

Promptless resume and fork keep their existing contracts. Output formatting
stays client-side. Forward supported execution overrides; reject unsupported
ones before queueing rather than silently drop them or change another client's
active turn.

Use the existing client-message identity to associate input with its actual
receiving turn. Expose that association to the observing CLI, including across
servers and a steer into an active turn. Add only the small input-to-turn receipt
needed for that lookup; normal turn status and history remain authoritative.
A queue-storage receipt alone is not proof of model admission.

Wait for that turn, not the next arbitrary final. Read its ordinary final/error
from canonical history if it finished before observation attached. A steered
turn's final may address other input too. Completed-result lookup must not load a
runtime or duplicate its transcript. Paginated history uses bounded metadata/item
pages and returns only the final ordinary assistant message. Legacy history
retains its existing full-history reconstruction compatibility path; that
temporary read is released before awaiting owner reconciliation, and the response
contains only the final message.

## Storage and retries

Extend the existing queued-input record with due time and fallback profile,
reusing its ID as the schedule identity and retaining the normal client-message
ID through admission. Keep one pending prompt, not separate immutable scheduled
text and editable queued text. The queue owns pending input; the normal turn
owns admitted execution. Derive running/completed/interrupted from that turn.
A failed dispatch only needs an error on the pending entry, not another job
state machine.

Ordinary queue editing changes that pending input and preserves its due time and
fallback profile. Deleting it is cancellation.
After consumption, the normal input-to-turn association supports result lookup;
there is no independently queued schedule to become stale or recreate it.
If neither pending input nor an admission receipt exists, report not found or
unconfirmed rather than wait forever for a nonexistent queued run.

Retry temporary storage, writer-contention, and resume failures using the same
pending entry and normal backoff/polling. Do not enqueue a fresh copy on every
attempt. Remove the pending entry after ordinary Core admission and retain only
the input-to-turn association needed by readers. A saved association identifies
an accepted input; it does not prove that all of its tool effects completed.

Creation-response loss is possible: the existing queue writes storage before
returning its RPC response, and a connection can close between those steps.
There is no automatic create retry today. Generate and retain the input identity
before the request; on an uncertain response, show that identity and ask the
caller to inspect the pending list/history before creating another schedule.
Do not add a separate idempotency-key service or automatic replay system.

Use ordinary queue crash/retry behavior, including its existing limits. The
client-message ID is correlation, not a new exactly-once guarantee. An ambiguous
admission can be checked through its input/turn receipt; do not claim that a
retry cannot duplicate external effects. There remains an ordinary queue crash
window between Core admission and the receipt/delete transaction. A crash there
can leave admission unconfirmed and input pending. A run with a durable receipt
that is later interrupted by server death or explicit unload is not rescheduled
as a new run. Existing conversation recovery may continue that same turn.

## Cancellation and thread lifecycle

Before the due time, removing the pending entry prevents scheduled delivery.
Once due dispatch has started, cancellation is best effort, like ordinary queue
deletion: another server may already hold the input. Report that it may still
run; do not promise atomic cancellation up to Core admission. Stop admitted work
through the existing interrupt/unload controls. No extra cancellation lock or
distributed transaction is required.

Input admitted from a schedule is ordinary text input. Rollback can target a
boundary before it using the normal history mechanism. Forks have new UUIDs and
do not inherit schedules aimed at the original thread. An archived target errors
as archived when delivery is attempted; a missing/deleted target errors as
missing. Scheduling never recreates it. Results follow normal history retention
and rollback visibility; report unavailable content when it is no longer there.

Client observation is independent of the accepted execution. CLI disconnect
releases its subscriptions without unloading the shared owning tree. For a
shared run, Ctrl+C requests interruption only when this invocation started that
turn; if it merely steered another active turn, Ctrl+C ends observation and
reports that the accepted input may still execute. Interrupting that shared turn
requires the ordinary explicit thread-interrupt action.

Explicit unload stops current turns and commands. It does not delete future
scheduled input, which authorizes a later normal resume when due. Respect an
unload in progress, retry normal admission afterward, and never resurrect an
already admitted/interrupted occurrence as a new task.

## Idle unloading and memory

Future input holds no Session, writer lock, model context, agent slot, fake
subscriber, or sleeping shell. Use an indexed due-time query in the existing
watcher. Keep existing queue/input limits and bounded history output.

Admitted queued work, running turns, owned commands, and accepted result
publication must participate in the normal activity/cleanup barriers. A
headless run cannot be evicted merely because it has no TUI subscriber. Creating
an internal listener is not permission to retain a fake client indefinitely.

After terminal publication, release input observers and runtime
references. If an independent thread has no real subscribers and is inactive,
allow ordinary idle unload after the configured delay, currently 30 minutes by
default. The delay runs from when both conditions hold, not from schedule
creation or the beginning of a long task. A returning TUI subscriber prevents
idle unload through the existing mechanism.

Agent-tree participants continue under the native control, residency, and
shutdown rules. Do not detach them or destroy a live tree merely to reclaim a
completed scheduled run. Conversely, a saved future schedule or result does not
pin that tree in memory.

## Implementation boundaries and qualification

Keep scheduling in the queue/extension, state, and app-server boundaries. TUI and
CLI use the same service; immediate exec shares its routing/result observation.
Do not add an execution engine or more orchestration to `codex-core`.

The scheduling additions are due-time metadata/filtering and selected-profile
resume for due unloaded targets. Headless execution additionally needs the small
input-to-turn association and result observation path. Keep cancellation, queue
editing, history, and shutdown under their existing owners.

Deploy and restart participating shared servers together, and update embedded
TUIs before relying on scheduled-input metadata. Compatibility with old queue
readers is outside this fork's initial scope; do not add consumer-version
negotiation. Update v2 APIs, schema fixtures, app-server README, CLI help, and
queue docs through the normal generation workflow.

Required behavioral coverage:

- Schedule creation resolves time/name/profile once; no prompt is delivered early
  or prevents eligible ordinary queue entries from running.
- Due-time checks work with unchanged database revisions, clock changes, and
  host sleep; shared and embedded owners use the same pending record.
- Due delivery loads an unloaded independent thread without a TUI, preserving its
  UUID, configuration, environment, and selected fallback auth.
- A loaded owner under another profile receives immediate exec and scheduled
  input using its own auth without starting a competing writer.
- Writer contention and unloading retry ordinary admission without bypassing
  exclusion or creating a fresh queued copy on every attempt.
- Active steering, idle starts, interrupted targets, and unsupported active tasks
  produce accurate admission and correlated results.
- A final before observer attachment, multiple inputs in one turn, lost
  acknowledgements, and CLI reconnect cannot return another turn's result.
- Restart before due and overdue pending work retain their input identity; an
  interrupted admitted run does not enter an automatic retry loop.
- Normal queue edits/deletion apply to scheduled input. Before-due cancellation
  prevents delivery; a dispatch race reports the weaker cancellation guarantee.
- Rollback treats delivered input normally; forks do not inherit schedules, and
  archive/delete errors do not recreate targets.
- Unloaded children, ephemeral targets, missing auth, unavailable environments,
  remote boundaries, and unsupported overrides retain ordinary validation.
- Headless turns and commands stay live while busy; completed independent runs
  release observers and unload under the configured idle policy. A connected
  TUI and native agent-tree safeguards retain their existing behavior.
- Pending lists and paginated history reads stay bounded, completed results do
  not load runtimes, legacy history uses its existing compatibility loader, and
  uncertain creation responses are not automatically resent.
- TUI behavior has snapshot coverage.

Use public app-server integration tests and CLI tests for routing and execution,
plus fake-clock/state tests for time and dispatch races.
Formatting and static review do not substitute for
the remote executable qualification required before release.
