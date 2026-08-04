# Multi-agent v1 response observation

Status: implemented in this fork

Related proposal: [Multi-agent v1 short targets](multi-agent-v1-short-targets.md)

## Summary

Multi-agent v1 should give `spawn_agent`, `send_input`, and `resume_agent` the same compact optional
response-observation field. The field lets the observer request the target's first commentary
response, wake when the target turn finishes, or explicitly avoid subscribing to that final
response.

The proposed model-facing flags are:

- `c`: observe the first subsequent commentary response.
- `f`: observe the target turn's final response and wake the sender if it is idle.
- `x`: keep the final response in the transcript without adding it to model context.

The field is additive. Omitting it preserves each tool's current behavior for the selected target
turn: the final response is delivered passively, but it does not wake an idle observer.

This is a compatible v1 extension, not a new multi-agent v3 contract. It keeps the existing
lifecycle tools, `wait_agent`, canonical thread UUIDs, and active-turn steering behavior. A mailbox
or observation registry may implement the behavior internally without becoming another
model-facing communication API.

## Implementation

The implementation keeps the wire policy, runtime observation state, and durable audit and
delivery state separate:

- `core/src/agent/response_observation.rs` parses the shared `w` field into a named policy.
- `core/src/agent/control/presentation/response_observation/` owns the per-observer aggregate,
  turn binding, stable delivery identities, wait ownership, and durable snapshots.
- `core/src/session/response_observation/` publishes only complete commentary items and final
  response events, resolves input admission to an exact target turn, and reconstructs target
  response state for live watcher replacement.
- V1 `send_input` forwards its task payload unchanged as `UserInput`. Direct inter-agent replies
  remain an explicit workflow in which the target UUID is supplied in orchestration context;
  routine acknowledgements and results use commentary and final-response observation without
  injecting sender UUIDs into target context.
- The TUI reserves `Main [default]` metadata for its primary thread, so child-side send and
  commentary rows remain readable even when app-server events carry only the parent's UUID.
- Core treats a bound `f` wake as pending automatic work when deciding whether to emit the
  thread-idle lifecycle callback used by active goals. Automatic idle-turn reservation repeats
  that check while holding the destination mailbox permit and observer transaction, so a target
  turn that binds during goal bookkeeping still wins before the active-turn placeholder.
- `RolloutItem::AgentResponseObservation` stores canonical, model-hidden observer UUID, target
  UUID, target turn, effective disposition, pending commentary admission cursors, and committed
  delivery IDs. A cursor combines a runtime event watermark with the preceding canonical
  commentary item ID; live recovery uses the item boundary because paginated model context does
  not preserve absolute rollout ordinals.

The watcher first flushes its pending delivery claim under the target lifecycle and observer
transaction. It releases the observer transaction before enqueueing the response, then releases the
target lifecycle as soon as that enqueue succeeds; neither boundary remains held while it waits for
the observer to consume the mailbox item. This matters when the observer is in a model response
that invokes another lifecycle tool: consuming the mailbox item may require that tool to finish,
while the later lifecycle call may need either boundary. At the next mailbox-consumption boundary,
the observer reacquires the transaction and flushes model-context delivery together with a freshly
computed committed observation suffix. A close that starts after enqueue revokes future
observation, but the response that already won admission remains durable through an inert committed
tombstone. Compaction and rollback append the current snapshots so a live orchestration instance
can replace a watcher without losing or duplicating delivery. An `x` observation instead emits the
same canonical model-hidden completion presentation used by passive and waking delivery, then
records a no-pending-delivery tombstone. The user, transcript, rollout, and client APIs retain the
final status and payload even though a later parent request does not include them in model context.
Cold resume and fork restore history but deliberately do not reactivate pending observations,
recreate child runtimes, or inspect child history to complete an old delivery.

Canonical completion rows state their relationship to the observer's model context:

- `<agent> completed (● visible)` uses green terminal styling and means the final response was
  added to this observer thread's model context through passive or waking delivery.
- `<agent> completed (○ not visible)` uses cyan terminal styling and means `x` retained the final
  response only for the observer transcript and clients.

The label is observer-relative. It does not claim that the response is hidden from the child
thread, user, rollout, or another observer. When an explicit `wait_agent` owns the completion, its
model-visible `Finished waiting` result owns presentation instead of emitting a duplicate
completion row.

V1 currently accepts full thread UUIDs as lifecycle targets. The names in prose examples are
descriptive labels only; nickname or compact-reference input remains owned by the parallel
short-target proposal. Observation registration happens only after the current V1 parser has
resolved the full UUID.

Successful child final responses are intentionally delivered in full. They are product-authored
agent context, like retained compaction content, and are not subject to the error-message
truncation allowance. The explicitly requested first complete commentary response is likewise
delivered in full; commentary is expected to be a short acknowledgement or clarification rather
than bulk output. Error payloads may still use the existing bounded error rendering.

## Motivation

V1 can already send several instructions to one running agent, and a completion can arrive in a
parent's active model turn without an explicit `wait_agent`. The current contract still leaves
three gaps:

1. The sender cannot request the target's useful first commentary response. This is important when
   the response acknowledges, interprets, or questions a steer before the target eventually
   completes.
2. The sender cannot request an automatic wake when an important target finishes after the sender
   has become idle.
3. The sender cannot state that a message is informational and that the target's eventual,
   potentially unrelated final response should remain user-visible without being injected into
   the sender's model context.

These gaps encourage unnecessary `wait_agent` polling and make sibling communication awkward.
V2 addresses related concerns with separate message, follow-up-task, wait, and mailbox concepts,
but that splits sending and response observation across a larger model-facing contract.

V1 can express the useful behavior by keeping one sending operation and adding a compact
observation policy.

The primary `x` workflow is child-to-parent coordination during a long child turn. A child can send
an important scope, contract, or implementation update to its parent immediately without
subscribing itself to the parent's later, usually unrelated completion. The completion remains
visible in the child's observer transcript for audit and manual inspection, but is not injected
into the observer's later model context. The parent otherwise sees the child's final response but
has no model-visible access to arbitrary mid-turn commentary unless it explicitly inspects the
child rollout. `cx` adds only the parent's first commentary acknowledgement or task-interpretation
reply to that one-way update.

Sibling-to-sibling coordination usually uses `cx`: the sending sibling receives the target
sibling's acknowledgement or task interpretation while continuing its own work, but does not
subscribe to the target's eventual final response. Use `x` instead when the sibling update is
strictly one-way.

## Goals

- Preserve each lifecycle tool's existing behavior when the new field is omitted.
- Let a sender receive the first coherent commentary response to a steer.
- Let a target-turn final observation survive the sender completing or starting other turns.
- Let informational messages avoid adding an unsolicited final response to model context.
- Keep explicit `wait_agent` useful for deadlines, multi-target waits, and late inspection.
- Support multiple observers of the same target turn independently.
- Preserve complete TUI, transcript, and rollout auditability even when model delivery is
  suppressed.
- Resolve all model-facing target aliases to canonical thread UUIDs before registering
  observations.
- Keep the tool-call representation compact enough to save output tokens in routine orchestration.

## Non-goals

- Replacing canonical thread UUIDs.
- Removing `wait_agent`.
- Adopting the full v2 mailbox and agent-path contract.
- Making reasoning summaries, tool progress, or arbitrary protocol events observable through `c`.
- Treating `x` as an unsubscribe or cancellation operation.
- Allowing a later informational steer to cancel an earlier requested final wake.
- Hiding agent activity from the user, transcript, rollout, or client APIs.

## Proposed tool field

The JSON tool schemas for `spawn_agent`, `send_input`, and `resume_agent` should add the same
optional compact string field. Its wire name is `w`. `send_input.w` carries the full process-focused
guidance; `spawn_agent.w` and `resume_agent.w` refer to it rather than repeating the same text in
model context. The schema intentionally leaves `w` as a string instead of enumerating values.
Runtime parsing remains authoritative for accepted values and model-visible validation errors.

The examples in this proposal use full UUIDs because that is the targeting syntax V1 currently
accepts. Persisted nicknames may replace them if the parallel short-target proposal is implemented:

```json
{
  "target": "019faa07-aa3d-78d3-9eca-66cd8626adad",
  "message": "Implement spec 123.",
  "w": "cf"
}
```

The schema description should explain the flags in full, while model-authored calls pay only for
the compact field and characters.

Accepted canonical values are:

- `c`
- `f`
- `cf`
- `x`
- `cx`
- `fx`
- `cfx`

The field should be omitted for the default mode. Unknown characters, duplicates, and noncanonical
ordering should produce a model-visible validation error rather than being silently ignored.

Internally, parse the wire value into a named observation-policy type. Do not propagate raw
characters or positional booleans through the implementation.

All three tools should route through the same observation-policy parser and registry:

- `spawn_agent.w` observes the spawned agent's initial turn.
- `send_input.w` observes the target turn that accepts the input.
- `resume_agent.w` observes the target's active turn, or its next turn if the resumed target is
  currently idle or already completed and the policy requests commentary or model delivery. A
  bare `x` returns the synchronous status without retaining a next-turn observer.

The synchronous tool result remains available regardless of `w`. For example, `resume_agent` may
return the saved result of a previously completed turn; `x` controls future event delivery and
does not erase that direct tool response.

## Per-call semantics

Each accepted call contributes a commentary request and a final-delivery disposition:

| `w` value | Commentary | Final disposition |
| --- | --- | --- |
| omitted | none | passive |
| `c` | first subsequent commentary | passive |
| `f` | none | wake |
| `cf` | first subsequent commentary | wake |
| `x` | none | presentation-only |
| `cx` | first subsequent commentary | presentation-only |
| `fx` | none | passive |
| `cfx` | first subsequent commentary | passive |

`f` and `x` cancel within the same call. Consequently, `fx` is equivalent to omitted mode and
`cfx` is equivalent to `c`. The parser accepts these combinations so mechanically combined flags
remain harmless, but tool guidance should recommend the shorter canonical equivalent.

The final dispositions mean:

- `presentation-only`: publish the final response to clients and durable history without adding it
  to model context.
- `passive`: make the final response available through the current non-waking delivery behavior.
- `wake`: deliver the final response and start a sender turn if the sender is idle.

`x` suppresses model-context injection only when no earlier or concurrent call already requests
final delivery for that observer and target turn. It does not hide completion from the TUI or
persisted history.

## Commentary observation

`c` observes the first completed assistant message with commentary phase after the observation
policy binds to the target turn.

It does not observe:

- Reasoning summaries or raw reasoning.
- Tool calls and tool progress.
- Status labels such as file discovery, test execution, or retries.
- The target's final answer unless that answer is itself the first subsequent commentary item.

Commentary observation is one-shot and associated with one observation request from
`spawn_agent`, `send_input`, or `resume_agent`. If several pending `c` requests from the same
observer are followed by one commentary item, that item should be delivered once to the observer
and satisfy all requests that were pending before it. The system cannot reliably attribute
free-form assistant commentary to one of several steers admitted at the same safe boundary, so it
should not manufacture stronger attribution.

When the observer is active, commentary is injected like a steer. When the observer is idle, `c`
wakes it and starts a turn. The delivered envelope must identify the source agent and target turn
so the model does not mistake inter-agent commentary for user input.

### Representative rollout sample

A small exploratory scan reviewed 20 recent V1 `send_input` interactions for which the sender call,
target rollout, first subsequent complete commentary item, and later final answer could all be
correlated:

| First commentary classification | Count |
| --- | ---: |
| Materially useful to the sender | 5 |
| Progress narration or acknowledgement without decision value | 9 |
| Substantially duplicated or adequately represented by the final | 6 |

The useful cases primarily confirmed changed scope, ownership, constraints, or interpretation.
Most low-value cases announced an intended audit, wait, or execution step, while the final answer
contained the actionable result.

This is a qualitative sample of recent lifecycle tests and code-review workflows, not usage
telemetry or a statistically representative corpus. It supports making `c` explicit and one-shot
rather than forwarding all commentary by default. It also supports observing the first complete
item: the immediate acknowledgement sometimes enables course correction, while later progress
messages are usually noise.

## Live durable target-turn observations

Final observation is aggregated over:

```text
(observer thread, target thread, target turn)
```

It is not scoped to the observer's current turn. The observer may finish, be resumed by the user,
compact, or begin unrelated work in the same live orchestration instance while its requested final
wake remains active. A process restart, cold resume, or fork is an explicit reconfiguration
boundary and does not restore that wake.

For one observer and target turn, aggregate final dispositions monotonically:

```text
presentation-only < passive < wake
```

Once any accepted lifecycle operation contributes `wake`, later `x`, `cx`, passive, or omitted
calls bound to the same target turn cannot downgrade it.

The observation ends automatically after that target turn reaches a terminal state and its
requested delivery is durably handled. A later turn started directly in the target thread is not
delivered to the old observer. The observer must use a new `spawn_agent`, `send_input`, or
`resume_agent` call to observe that later work.

Explicit `close_agent` revokes any still-pending presentation-only, passive, or wake observation
for the closed target and its live descendants across every live observer. Shutdown therefore
cannot deliver a queued final response or silently recover an old V1 watcher. Child publication
and reopening serialize with the direct parent's close boundary, preventing a late child runtime
from remaining beneath a closed parent. The same close rule applies when the target uses the v2
orchestration path. Revocation also removes any still-unclaimed completion-context authorization
owned by the closed child, so a queued wake cannot become model-visible after close supersedes it.

`resume_agent` and `close_agent` reject the caller's own thread UUID before changing observation or
lifecycle state. Resuming the current runtime is meaningless, while closing it from its own tool
turn would wait for a shutdown that cannot finish until that turn returns. An agent finishes its own
work by returning its result; another agent may close the completed thread afterward.

If close invalidates a lifecycle generation after a foreign observer has already scheduled watcher
recovery, that recovery revokes only its obsolete presentation before stopping. A fresh
post-close presentation for the same canonical thread UUID is a separate explicit observation and
remains intact.

For example:

```json
{
  "target": "019faa07-aa3d-78d3-9eca-66cd8626adad",
  "message": "Implement spec 123.",
  "w": "cf"
}
```

This creates a durable final wake for the target's active turn. A later update may use:

```json
{
  "target": "019faa07-aa3d-78d3-9eca-66cd8626adad",
  "message": "Spec 123 changed in sections 5 and 7.",
  "w": "cx"
}
```

When both calls bind to the same target turn, the second requests a commentary acknowledgement and
contributes a presentation-only final disposition. It does not cancel the earlier `f`; that turn's
final response still wakes the observer.

The aggregate is cleared only after that target turn reaches a final state and its requested
delivery has been durably handled. Inputs admitted to a later target turn start with a fresh
aggregate.

## Binding to the target turn

Observation state must bind to a specific target turn rather than to a process-local watcher or
the observer's current turn:

- `spawn_agent` binds when the initial child turn is created.
- `send_input` binds when the input is admitted to a target turn, not when the tool call begins.
- `resume_agent` binds immediately when the target has an active turn. If the target is idle or
  completed, it retains commentary, passive, or wake handling for the next target turn that
  starts. A bare presentation-only `x` records the synchronous status and does not retain a
  next-turn observer.

A full-history spawn can expose the parent's active turn in the child history while the initial
child input is still being admitted. Future-turn observation remains pending across that inherited
turn and binds only to the new child turn. This includes omitted `w`: the child's final response is
delivered passively without starting a parent turn, and the next user-initiated parent turn receives
it in model context.

A completed result replayed synchronously by `resume_agent` is historical tool output, not a new
target-turn completion, and does not consume the policy intended for the next turn.

For `send_input`, admission-time binding matters when:

- The target is idle and accepting the input starts a new turn.
- The target finishes concurrently with `send_input`.
- An interrupt aborts the current turn and the input starts its replacement.
- Several queued inputs are accepted together at a safe boundary.

The observation registry should use the resolved target turn ID returned by admission. It must
not guess from a status snapshot taken before submission.

Admission to a still-active regular turn is also a commitment to sample that input. If the task
has already made its last pending-input check but has not yet atomically detached from the active
turn, finalization continues the same turn and samples the newly accepted input without emitting
another turn-start lifecycle. This closes the narrow post-response race in which the input and its
`c` observation could otherwise bind to a finishing turn, be persisted after its final answer,
and remain unseen until unrelated later input started another turn. Inputs intentionally deferred
at an MCP-use boundary or by queue-only delivery retain their existing next-turn behavior and do
not force this continuation.

Turn admission records that canonical ID and `Running` status in the target's live response state
before the asynchronous task publishes `TurnStarted`. Admission, `TurnStarted` response/status
publication, and removal-generated final-outcome publication share the final-outcome boundary. If
admission wins, removal emits NotFound for the admitted turn; if removal wins, the task and its
later events cannot publish from the removed runtime. A lifecycle request that finishes against an
Arc after that runtime was removed or replaced reports `ThreadNotFound` rather than treating work
on the stale instance as accepted. Publishing the admitted turn identity and `Running` together
prevents a prior turn's final status from being mistaken for the newly admitted turn.

A prior turn may still finish publishing after the new admission boundary. Its turn-scoped
final outcome remains available to observers of that prior turn, but it cannot replace the newer
turn's session-wide `Running` or completed status, nor make removal preserve the historical result
as current. Live response reduction and canonical reconstruction retain the latest admitted turn
as the authoritative status identity after that turn becomes inactive.
Conversely, if the current turn's final-outcome presentation already made status final but removal
overtakes durable event publication, removal settles the same turn in response state before
fencing the delayed event.

If several `send_input` calls contribute to one target turn, the target still produces one final
response. The observer receives that response once according to the aggregate disposition.

## Sender lifecycle

Within one live orchestration instance, the observer's turn lifecycle does not cancel a
target-turn observation:

- If the observer is active when commentary or a subscribed final arrives, inject it into that
  turn like a steer.
- If the observer is idle and the event has wake behavior, start an observer turn.
- If the observer completed and the user later resumed it, inject into the newly active turn or
  wake it again if it has returned to idle.
- If the observer is compacted or a live watcher is replaced before delivery, restore the
  outstanding observation and its delivered/not-delivered state within that live instance.

Restoration must be idempotent. A stable delivery identity such as:

```text
(observer thread, target thread, target turn, event kind, source item ID)
```

should prevent duplicate automatic injection after rollback, compaction, or live watcher
replacement.

Cold resume and fork are different. They restore conversational and audit history, but discard
pending commentary observations and presentation-only, passive, or wake-capable final
observations. The user or model must explicitly call `resume_agent`, `send_input`, or `spawn_agent`
after evaluating the current situation.

## Interaction with active goals

An active goal normally starts its next automatic turn as soon as the current thread becomes idle.
When an `f` observation is already bound to a concrete target turn, that immediate continuation
would race the subscribed result and often cause an unnecessary `wait_agent` turn.

While such a bound wake remains outstanding, Core defers the thread-idle lifecycle callback. The
target's final response then starts the observer's next turn directly, with both the subscribed
result and active-goal context available to the model. When that turn finishes, normal goal
continuation resumes if no other bound final wake remains.

The idle callback's initial check is only a fast path. Before reserving an automatic turn, Core
serializes with target-turn binding and response delivery, rechecks both trigger mail and bound
final wakes, and only then installs the active-turn placeholder. A wake bound after that
placeholder was installed belongs to work that became observable later and may steer the already
reserved turn.

This deferral does not apply to an unbound `resume_agent(w: "f")` policy waiting for an idle
target's hypothetical next turn. That target may never start more work, so an unbound policy
cannot indefinitely suppress goal progress. Commentary-only `c` and passive final observations
also do not defer goal continuation because neither guarantees a future result that can replace
it. Explicitly closing a target revokes its wake and re-evaluates idle lifecycle for affected live
observers, including observers other than the agent that issued `close_agent`.

Thread-idle lifecycle is level-triggered rather than an exactly-once event stream. Observation
cleanup and ordinary turn completion may both probe the same idle state. Active goals remain
single-flight because `GoalRuntime` holds its goal-state permit through the continuation decision,
and Core atomically reserves the idle turn before releasing that permit. A second probe therefore
either sees the active turn or loses the same reservation race; it cannot start a duplicate goal
turn.

## One-shot `codex exec` lifecycle

`codex exec` exposes the multi-agent tools, but its host lifecycle ends when the primary turn
completes. It does not remain attached to the now-idle thread to receive a later wake.

Consequently:

- `c` remains useful when the target's first complete commentary arrives while the primary turn is
  active; it is injected as a steer.
- An observed final that arrives while the primary turn is active may likewise be injected into
  that turn.
- `f` provides no additional idle-wake behavior in `codex exec`. Once the primary turn completes,
  the process exits instead of starting another turn for a later target final.
- `wait_agent` remains useful because the model calls it inside the active primary turn. Its tool
  result continues that turn; it is retrieval, not an idle wake.

For example, `w: "cf"` in `codex exec` still requests the useful commentary acknowledgement, but
the `f` portion cannot wake the process after primary-turn completion. Long-running headless
orchestration should keep the primary turn active with explicit waits or use a host that remains
attached to the thread.

## Existing target-turn observations

Spawn, input, and resume must use one observation mechanism rather than maintaining special-case
listeners for each tool. Several lifecycle calls may bind presentation-only, passive, or
wake-capable observation to the same target turn.

All calls for the same observer and target turn should participate in the same monotonic
aggregate. Therefore:

- `x` means "this operation keeps final presentation out of model context."
- `x` does not cancel final observation created by spawn, resume, or an earlier input for that
  target turn.
- A model-context fire-and-forget result occurs when the effective aggregate remains
  `presentation-only`.

Using `w: "x"` on `spawn_agent` explicitly creates model-context fire-and-forget work while keeping
the initial target turn's final response visible to the parent transcript. Using `w: "x"` on
`resume_agent` presents an active target turn's final response without injecting it; when the
target is already idle or completed, bare `x` returns its synchronous status without retaining a
next-turn observer. A later `send_input` with `w: "f"` may still upgrade that observer's
disposition if it binds to the same target turn.

Overloading a later `send_input` with `w: "x"` as an unsubscribe would remain surprising and
unsafe.

## Multiple observers

Observations are per observer. One target turn may deliver its commentary or final response to
the spawning parent, a user-resumed agent, and one or more sibling agents.

One observer's `x` must not suppress another observer's delivery. One observer's `f` must not wake
every listener. Each delivery must preserve the source target UUID even if the observer used a
nickname or future compact reference.

This enables pair workflows without explicit polling:

- A backend and frontend coder can exchange interpretation updates with `c`.
- A supervisor can request a final report with `f`.
- A coder can send an informational update to a supervisor with `x`.
- A user-resumed coder can observe a delegated reviewer while the supervisor independently
  validates the same review.

Live UUID adoption adds an observation edge; it does not change tree membership. Setup snapshots
the target under its own lifecycle boundary, releases that boundary before holding the observer's,
and validates the exact target generation and presentation before committing durable observation
state. Mutually observing agents therefore cannot deadlock by each holding the other's lifecycle
guard.

## Cold resume, fork, and historical child IDs

History is restored across `codex resume` and `codex fork`; live agent relationships are not.
This boundary is intentional:

- A cold resume may occur long after the original work, when the repository, task, or external
  state has changed.
- A fork needs branch-specific instructions before it contacts agents referenced by the shared
  history.
- Neither operation should silently recreate child runtimes, reopen target rollouts, or deliver an
  old pending `c`, passive, or `f` response.

A fork receives a new orchestrator thread UUID, so its old observation records do not identify the
new observer. However, collaboration tool history can still contain the full UUIDs of child
rollouts created before the boundary. Those UUIDs are historical references, not inherited
observations.

An explicit lifecycle call may deliberately reuse one of those UUIDs. `resume_agent` then resumes
the original child rollout rather than cloning it, so the original thread and multiple forks can
all address the same child and affect its single conversation. Callers that need branch isolation
should spawn a new child. Exclusive adoption, cloning, or branch-local aliases for historical
children are separate capabilities and are outside this proposal.

## Interaction with `wait_agent`

`wait_agent` remains useful for:

- Applying a timeout or context deadline.
- Waiting on several agents.
- Inspecting a result after an unexpectedly long task.
- Explicitly retrieving a result after automatic delivery was suppressed.

An active `wait_agent` for the same observer and target turn should claim that final-response
delivery.
If the wait returns the target's final response, it satisfies the outstanding `f` without starting
another automatic wake. If the wait times out, the target-turn observation remains active.

This arbitration also applies when the child and observer use distinct `AgentControl` instances,
such as a V1 parent adopting an independently resumed child. The child records each live
subscriber's observer-local final-outcome presentation before exposing final status. A wait
registered after that recording but before its status snapshot claims the still-queued watcher
presentation, so one observer does not receive both the explicit result and a later automatic copy.

A watcher final-outcome presentation remains claimable after the delivery worker removes it from
the queue and while that worker is blocked on persistence, mailbox admission, or scheduling. The
presentation atomically commits either wait ownership or automatic ownership before emission,
closing the final race between the ownership check and rendering. Final-status observer
registration and its status snapshot are serialized with the same final-outcome publication
boundary, including live watcher recovery.

The final-status observation carries the target turn ID as well as its status. If several turns
from one child still have queued or in-flight presentations, `wait_agent` claims presentations
only for the exact turn it returned. Older subscribed finals remain available to their automatic
delivery workers.

Final-outcome deduplication is keyed by observer presentation, child presentation, and target turn
for the lifetime of the target-turn observation. A newer turn therefore cannot make a delayed
durable event recreate an older turn's presentation. If recovery temporarily reconstructs more
than one presentation for the exact returned turn, `wait_agent` claims every copy but renders and
persists that canonical `(child thread UUID, turn ID)` target once, including when the copies came
from different runtime presentations across a live reload.

Thread removal publishes its NotFound presentation and final lifecycle status while the runtime
remains hidden behind the thread-map write boundary. Readers therefore cannot observe map absence
before an already-active wait has an opportunity to claim the final-outcome presentation. If the
target has an active turn, NotFound uses that turn's canonical ID so an already-bound
`spawn_agent`, `send_input`, or `resume_agent` observation receives the teardown final outcome.
Only an idle target without an active response-stream turn receives a synthetic final-outcome turn
ID.

Temporary residency unload deliberately suppresses final-outcome presentation and preserves the
target-turn observation for reload. Removal still closes the old runtime's observer streams so a
foreign observer can enter reload recovery when another `Arc` retains that runtime; it does not
advance the explicit-close lifecycle generation or inject a teardown result.
Residency eviction must first acquire the candidate's lifecycle boundary without blocking. A
candidate already owned by input admission, close, or reload remains resident for that reservation
attempt, preventing eviction from invalidating work accepted under the same boundary.

A completed wait item and its committed observation snapshots are flushed as one canonical batch.
If that flush reports an error after the batch committed, reconciliation uses the wait's stable
item and turn IDs through the same completion-presentation lookup used by standalone completion
messages. The wait therefore retains final-outcome presentation ownership instead of releasing the
same completion to automatic delivery.

This suppression applies only when the wait already owns the pending delivery. If the automatic
completion was delivered before `wait_agent` was called, a later explicit wait may return and
render the completed result again. The implementation should not retroactively erase an already
delivered event.

The observation policy belongs to the V1 caller rather than to the target's tool inventory. A V1
caller that addresses a live V2 thread by canonical UUID therefore observes the target through the
common response-event stream and receives the requested `c`/`f` behavior using V1 delivery and
durability semantics.

The existing "return on the first relevant target" behavior for a multi-target wait is separate
from this proposal. An explicit any/all wait mode is useful follow-up work, but it should not
expand this implementation series or change lifecycle-tool observation semantics.

## Auditability and model context

Observation policy changes model delivery, not persistence.

Regardless of `w`:

- The target input remains in the target rollout.
- The canonical collaboration tool item records the resolved sender and receiver UUIDs.
- V1 spawn, input, and resume items record whether the effective policy receives first commentary
  and wakes on completion; app-server exposes these as `observeCommentary` and
  `wakeOnCompletion`, and the TUI states both decisions.
- Commentary and final responses remain available to the user through the target transcript.
- Completion status remains visible through agent inspection.
- TUI presentation must not imply that an `x` completion was delivered to the observer model.

When commentary or a final response is injected into an observer, use a provenance-bearing
inter-agent envelope. Do not inject it as an unlabelled user message.
The exact commentary envelope remains in durable model context, while app-server transcript
projection converts it to a plain source-agent label and message. The TUI resolves that source UUID
to current agent metadata and must not expose the internal tag or JSON payload.

Non-paginated and paginated rollouts should preserve the same canonical observation and delivery
information. Raw function-call arguments alone are not sufficient because they contain the
model-authored target reference and cannot represent the eventual target-turn binding or resolved
UUID.

## Short targets and canonical identity

This proposal is parallel to `multi-agent-v1-short-targets.md`.

The two features should meet at one resolver boundary:

```text
model target
    -> resolve nickname, future compact reference, or full UUID
    -> canonical target ThreadId
    -> admit input to target turn
    -> register observation using canonical observer/target/turn IDs
```

Subscription state must never be keyed by nickname, task name, partial UUID, or process-local
registry position.

The short-target proposal initially prefers persisted nicknames. A future deterministic compact
reference may be added if it:

- Is persisted with its UUID mapping.
- Survives cold resume.
- Is never recycled within the root lineage.
- Resolves before lifecycle and observation logic.
- Leaves full UUID targeting available.

The model-facing `spawn_agent` result may eventually prefer the compact reference, while canonical
events, TUI metadata, client APIs, and rollout audit records retain the UUID.

Persisting an identity mapping across cold resume does not reactivate any observation
that previously referred to that identity.

## Relationship to multi-agent v2

This is a v1 extension.

V2 may reuse the same internal observation registry, mailbox storage, delivery deduplication, and
target-turn binding. Its existing `send_message`, `followup_task`, and wait schemas need not change
as part of this proposal.

A v3 would be justified only by intentionally replacing both public tool sets with a new
orchestration contract. Adding an optional, backward-compatible response policy to V1 does not
justify another configured multi-agent version.

## Persistence and recovery requirements

Outstanding wake observations must remain valid until their target turn terminates, including
across:

- Observer turn completion.
- Observer user follow-ups.
- Observer and target compaction.
- TUI agent switching.
- Target thread unload and reload while the observer and observation registry remain live.

Persist enough canonical state for live recovery, delivery commits, and later audit:

- Observer thread UUID.
- Target thread UUID.
- Target turn UUID.
- Effective final disposition.
- Pending commentary observation state, including its preceding canonical source-item boundary.
- Delivery identities already committed.

Do not persist a process-local channel, watcher handle, nickname lookup result, or registry index
as identity.

During live watcher replacement, recovery should reconcile against the target's canonical response
state:

- If the target is still active, restore observation.
- If the target completed and delivery was not committed, perform the requested delivery once.
- If delivery was committed, do not inject it again.
- If the target or its turn cannot be recovered in the live instance, retain an auditable warning
  and allow explicit UUID-based inspection rather than retargeting the observation.

Transient canonical-history read failures are retryable while the observer and target lifecycle
generation remain current. Recovery uses capped backoff rather than treating a store failure as an
empty history and waiting indefinitely for an unrelated runtime reload.

If persisting a target final-response event fails while the process remains live, publish that
final outcome to existing response observers so their presentation-only, passive, or wake delivery
does not remain blocked forever. The observer-side delivery must still pass its own durable commit
boundary. Commentary remains unobservable when its source item was not persisted, because
forwarding unauditable intermediate text is not required for final-response delivery liveness.

Once a target terminal event reserves accepted completion delivery, its canonical presentation
retry retains that reservation. Graceful shutdown waits for the retry to persist the stable
presentation item before terminating, including for presentation-only `x` delivery. This is
distinct from reconstructing an unresolved observation after the process has already crossed a
cold boundary.

On cold resume or fork, persisted observation records remain model-hidden audit and idempotency
history only. Do not turn pending records into watchers, automatic model-context items, or child
runtime reconstruction. An explicit lifecycle tool call establishes any new observation after the
boundary.

## Suggested model guidance

The tool description should convey these rules concisely:

```text
Optional response handling for target agent turn. Omit for normal passive delivery. c: receive first commentary reply, such as acknowledgement or task interpretation. f: receive final reply automatically. Continue parallel work, or finish your current turn to wait; completion wakes you when idle. f and wait_agent are alternatives for same target turn. x: do not subscribe this call to final reply; use to notify parent mid-turn so parent's later completion is not injected into current task. Can combine as cf or cx.
```

`spawn_agent.w` and `resume_agent.w` use:

```text
Same response handling as send_input.w.
```

Examples:

```json
{"message":"Investigate the failure and report only if asked.","fork_context":false,"w":"x"}
```

```json
{"target":"019faa07-aa3d-78d3-9eca-66cd8626adad","message":"Implement the approved change.","w":"f"}
```

```json
{"target":"019faa07-aa3d-78d3-9eca-66cd8626adad","message":"Before continuing, confirm how you interpreted section 5.","w":"c"}
```

```json
{"target":"019faa01-bb4e-79d4-8fcb-77de9737beef","message":"The API contract now uses cursor pagination; I am updating callers.","w":"x"}
```

```json
{"target":"019faa07-aa3d-78d3-9eca-66cd8626adad","message":"Check whether this API shape works for your frontend.","w":"cf"}
```

```json
{"target":"019faa01-bb4e-79d4-8fcb-77de9737beef","message":"The shared API shape changed; acknowledge if this conflicts with your plan.","w":"cx"}
```

```json
{"id":"019faa07-aa3d-78d3-9eca-66cd8626adad","w":"cf"}
```

## Required coverage

Implementation should cover:

- Omitted `w` preserving current model-visible and wake behavior for spawn, input, and resume.
- All three lifecycle tools using the same parser, aggregate, persistence, and delivery machinery.
- Each accepted flag combination producing the documented per-call disposition.
- Invalid characters, duplicate characters, and noncanonical ordering returning a model-visible
  error.
- `c` delivering one coherent commentary item, excluding reasoning and tool-progress events.
- `c` waiting for the first complete commentary item rather than forwarding streaming fragments.
- Several pending `c` requests being satisfied by one commentary without duplicate injection.
- `c` injecting into an active observer and waking an idle observer.
- A pending `c` or `f` mailbox delivery not blocking the observer from closing the same target
  before that delivery reaches the next model-input boundary, while preserving the response that
  already won admission.
- `f` surviving observer turn completion, user follow-up, compaction, and live target
  unload/reload.
- `cf` followed by `cx` retaining the original final wake.
- `x` alone producing no observer model-context item while preserving TUI and rollout history.
- `spawn_agent` with `x` performing true fire-and-forget work.
- `resume_agent` with `x` returning its synchronous status without subscribing to the next final.
- `resume_agent` with `c` or `f` binding to the active target turn, or the next target turn when
  resumed from an idle or completed state.
- `fx` behaving like omitted mode and `cfx` behaving like `c`.
- An omitted or passive call not downgrading an existing `f`.
- A final target response being delivered once after several inputs in the same target turn.
- Observation state and its watcher clearing at the target turn boundary.
- A later turn started directly in the target thread not reaching the old observer, followed by a
  fresh lifecycle call observing a subsequent turn.
- Watcher retirement racing a new lifecycle call preserving the newly admitted target-turn
  observation.
- An input racing target completion binding to the turn that actually accepts it.
- Interrupt input binding to the replacement target turn rather than the aborted turn.
- Multiple observers receiving independent delivery according to their own aggregates.
- Two live agents concurrently adopting one another without cyclic lifecycle locking.
- A wait owning an outstanding final delivery without causing an extra wake.
- A wait claiming a watcher final-outcome presentation after the delivery worker has removed it
  from the queue but before automatic delivery takes ownership.
- Observer registration racing final-outcome publication either receiving the pre-status callback
  or snapshotting the final status, never missing both.
- Thread removal publishing NotFound for the active target-turn ID to a bound response observation,
  retained runtime, and already-active wait before map absence becomes observable.
- Thread removal racing an admitted but not-yet-published `TurnStarted`, with the canonical turn
  receiving NotFound and the delayed start unable to restore `Running`.
- A delayed historical final outcome remaining observable by its turn ID without replacing a
  newer admitted turn's `Running` or completed status, both live and after reconstruction.
- Removal overtaking durable publication after final-outcome presentation settling the same final
  turn in response state rather than leaving it active.
- Completed target-turn delivery retiring a foreign-control watcher even while another `Arc`
  retains the old runtime and its completed status.
- Explicit close invalidating an already-scheduled foreign recovery without deleting a fresh
  post-close presentation for the same thread UUID.
- Temporary V2 residency unload closing a retained old runtime's observer streams and rebinding a
  foreign V1 watcher after reload without injecting a teardown result.
- V2 residency skipping a completed candidate while another lifecycle operation owns its boundary,
  then evicting it after that boundary is released.
- A late wait observing the second of two completed child turns claiming only that turn while the
  first final remains available to automatic delivery.
- A delayed durable final for an older turn remaining deduplicated after a newer child turn, with
  an exact-turn wait co-claiming any recovery-created copies.
- Live watcher recovery preserving the pre-status callback for independently controlled children.
- Live watcher recovery retrying a transient canonical-history read and delivering the durable
  completion without requiring target reload.
- Pending final wake delivery surviving actual parent compaction in non-paginated and paginated
  history.
- A fork leaving inherited commentary/final observation records inert until the fork explicitly
  reuses the original child UUID, in non-paginated and paginated history.
- A V1 observer applying `c`/`f` policy to a live V2 target addressed by canonical UUID.
- A timed-out wait leaving the final wake active.
- A wait called after automatic delivery being allowed to return/render the completed result
  again.
- A bound final wake deferring thread-idle goal continuation through the complete wake turn, then
  resuming it after observation cleanup even when persistence finishes later.
- Explicit close and ordinary target-turn watcher teardown removing the last bound wake from an
  idle foreign observer and re-evaluating its level-triggered idle lifecycle without starting
  duplicate goal turns.
- Explicit `wait_agent` retrieving a result after an `x` call.
- `codex exec` accepting mid-turn commentary and final delivery, exiting at primary-turn
  completion instead of honoring a later `f` wake, and retrieving results through an in-turn
  `wait_agent`.
- Full UUID targets being registered canonically; when nickname or compact targeting is added, its
  resolver must run before observation registration.
- Non-paginated and paginated rollouts reconstructing equivalent observation state and transcript
  presentation.
- Resume and fork reconstruction excluding response events removed by exact rollback ranges.
- Cold resume and fork not reactivating pending commentary, passive final delivery, or final wake.
- Explicit `resume_agent`, `send_input`, or `spawn_agent` establishing a fresh observation after
  a cold-resume or fork boundary.
- Historical child UUIDs in forked history remaining inert until explicitly reused, with explicit
  reuse addressing the original shared child rollout rather than cloning it.
- Registration persistence failures restoring and durably superseding the pre-call observation
  state for both `send_input` and `resume_agent`.
- Delivery deduplication across rollback, compaction, and live watcher replacement.
- V1 spawn, input, and resume history rows showing commentary and completion-wake decisions
  independently; non-V1 observation tools leaving both fields unavailable.
- V1 commentary rendering as a normal named-agent notification in live, replayed, and
  full-transcript TUI history without exposing the model-context envelope or its JSON payload.
- V1 send input identifying the sender by canonical UUID while leaving the initial spawn prompt
  unchanged.
- A steer accepted after a regular task's final pending-input check continuing and sampling in the
  same turn, with one user-message lifecycle and no repeated turn-start lifecycle.

## Decisions from design review

- Keep compact wire field name `w`. Put process-focused flag guidance on `send_input.w`, reference
  it from `spawn_agent.w` and `resume_agent.w`, and leave runtime parser responsible for validation
  instead of repeating an enum in every model-visible schema.
- Apply the same policy to `spawn_agent`, `send_input`, and `resume_agent`; do not retain separate
  implicit listener implementations when the shared observation mechanism can express them.
- Expose only the first complete commentary item. Streaming commentary and additional progress
  modes are outside the initial contract.
- Treat an explicit any/all mode for multi-target `wait_agent` as useful follow-up work rather
  than part of this implementation series.
