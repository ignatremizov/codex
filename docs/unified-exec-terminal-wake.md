# Unified exec terminal completion wake

Status: deferred proposal

Implementation should begin only after the `/agent <uuid> <prompt>` TUI work lands.

Related implementation: [Multi-agent v1 response observation](multi-agent-v1-response-observation.md)

## Summary

Unified exec should support a one-shot process-exit observation that can wake an idle model turn.
This removes routine `write_stdin` polling as the only way to make background command completion
model-visible.

Increasing `yield_time_ms`, exposing its resolved default, or explaining that its input type is
`u64` can improve polling behavior, but those changes address a symptom. The underlying gap is
that unified exec already observes process exit for TUI presentation while it has no corresponding
model-context delivery path.

The proposed model-facing form is:

```json
{
  "cmd": "long-running-command",
  "w": "f"
}
```

`write_stdin` should accept the same field so an existing process can be subscribed after spawn:

```json
{
  "session_id": 42,
  "w": "f"
}
```

`f` means: if this tool call returns while the process is still running, deliver that process's
terminal result later and wake the observing thread if it is idle.

## Goals

- Let a model request one terminal completion without repeated polling.
- Preserve current behavior when `w` is omitted.
- Keep command output streaming and terminal presentation visible in the TUI.
- Deliver process completion into model context exactly once.
- Inject completion into an active observer turn or start a wake turn when the observer is idle.
- Persist the accepted terminal result before making it model-visible.
- Reuse the response-observation admission, delivery, and deduplication invariants where practical.
- Keep `write_stdin` for interactive input, explicit progress inspection, and bounded synchronous
  waits.

## Non-goals

- Replacing `write_stdin`.
- Forwarding every output chunk into model context.
- Treating terminal output as commentary observation.
- Keeping processes alive across thread shutdown, process restart, cold resume, or fork.
- Providing a durable cron, reminder, or operating-system scheduling service.
- Allowing one Codex thread to subscribe to another thread's terminal sessions.

## Model-facing contract

Add optional `w` to `exec_command` and `write_stdin`.

Initially, only `f` has useful terminal semantics:

- omitted: preserve current synchronous result and TUI-only later completion behavior.
- `f`: request one model-visible terminal result and an idle wake.

Agent-observation `c` does not map to terminal output. Terminal streaming is usually progress noise,
not a coherent acknowledgement. Agent-observation `x` is also unnecessary initially because
omitting `f` already leaves later completion out of model context while retaining normal TUI
presentation.

The flag does not leave a Responses API function call unresolved. Each tool call returns its normal
result. If the process is still running at that boundary, the runtime retains a separate one-shot
subscription.

`w` must not silently change `yield_time_ms`:

- `exec_command` keeps its bounded initial wait.
- `write_stdin` keeps its requested or resolved wait.
- A later design may add an explicit immediate-subscribe behavior if waiting once before
  subscription proves unnecessarily expensive.

## Lifecycle

```text
exec_command(..., w: "f")
    |
    +-- process exits before the tool returns
    |      |
    |      +-- return the terminal result directly
    |      +-- do not retain or emit a second wake
    |
    +-- process is still running when the tool returns
           |
           +-- return the session id and current output
           +-- retain a one-shot terminal subscription
           +-- let the observer continue parallel work or finish its turn
                    |
                    +-- process exit watcher claims completion
                           |
                           +-- persist terminal observation
                           +-- observer active: enqueue into that turn
                           +-- observer idle: reserve and start a wake turn
```

`write_stdin(..., w: "f")` follows the same terminal claim rules for an already running process.

The subscription is bound to the concrete process generation and observing Session instance, not
only the numeric process id. Process ids can be reused, and a cold Session with the same thread UUID
must not inherit an old live subscription.

## Terminal observation

The model-visible observation should identify:

- originating command;
- unified exec process id;
- original tool-call identity;
- terminal status and exit code or failure;
- final output not already delivered to the model;
- truncation or omission metadata.

The output must reuse unified exec's existing bounded output representation. A wake must not create
an unbounded model-context item or expose output that ordinary unified exec handling would redact.

The observation needs a stable core-authored identity. It should be durable before an idle wake is
started, survive rollback or compaction once delivered, and remain distinguishable from
user-authored text. TUI projection should reuse the existing terminal row rather than render a
duplicate command completion.

Pending subscriptions are live runtime state only. Delivered observations are durable history.
Cold resume and fork retain delivered history but do not recreate pending subscriptions or
processes.

## Claim and deduplication

Synchronous retrieval and background delivery race for one terminal result. Exactly one path must
win an atomic claim:

- `exec_command` sees terminal state before returning: direct tool result wins.
- `write_stdin` sees terminal state: that tool result wins.
- process exit occurs after a still-running tool result: watcher delivery wins.
- a watcher and `write_stdin` observe exit concurrently: one claims delivery; the other reuses or
  reports the already committed result without injecting a duplicate wake.

A committed terminal observation remains available for transcript and audit purposes even when a
later explicit inspection renders the same status again. Model-context delivery remains one-shot.

Multiple `w: "f"` calls from the same observer for the same process generation should merge
idempotently. They must not schedule multiple wake turns.

## Cancellation and shutdown

- Interrupting a bounded `write_stdin` wait does not cancel an already registered `f`
  subscription.
- Explicitly stopping the background terminal cancels its pending wake before terminating it.
- Closing or shutting down the observing thread cancels every pending terminal subscription.
- Session shutdown continues to terminate unified exec processes.
- A fork receives neither the process nor its subscription.
- A cold resume receives neither the process nor its subscription.

Whether a user-requested terminal stop should produce a presentation-only cancellation observation
remains an open product decision. It must not wake the model after the user explicitly stopped the
work.

## Active turns, goals, and user input

Terminal delivery should use the same high-level scheduling rules as agent final-response
observation:

- When the observer is active, queue the terminal observation for the next safe model-input
  boundary without starting another turn.
- When the observer is idle, reserve one automatic wake turn.
- A bound terminal wake counts as pending automatic work for `/goal`, so goal continuation does not
  start a competing turn.
- User input does not duplicate or consume the terminal result accidentally.
- Explicit cancellation remains the only way to revoke the subscription before terminal state.

The implementation should generalize async response delivery where possible instead of introducing
a second independent wake scheduler beside agent response observation.

## TUI presentation

The originating command row should indicate that process-exit wake was requested. The terminal row
should indicate whether its result was added to the observer's model context, using the same
observer-relative visibility language as agent completion rows.

TUI output streaming remains independent of model observation. The user can continue to inspect
live output, switch threads, interrupt the model's current wait, or stop the process.

The completion event must not create a second command row or replay stale running state after thread
switching.

## Existing polling schema groundwork

Core tool planning has the current `TurnContext`, and `WriteStdinHandler` now carries the resolved
default into its model-visible spec.

Before terminal wake exists, the compact dynamic schema exposes the effective default:

```text
Writes chars to or polls an exec session, returning recent output. Polls return immediately on process exit; prefer long yield_time_ms values to repeated polls.
```

```text
Maximum poll wait in uint64 ms, independent of exec_command’s initial wait. Omit for default {yield_time-ms-default} ms
```

The placeholder is resolved from the current turn configuration when the tool schema is built.
Once wake exists, schema guidance should prefer `w: "f"` for completion dependency and reserve
long `yield_time_ms` values for genuinely synchronous waiting.

## Required race coverage

Integration coverage should include:

1. Process exits inside the initial `exec_command` wait: one direct result, no wake.
2. Process survives the initial wait and later exits while the observer is idle: one wake turn.
3. Process exits while the observer has an active turn: one injected observation and no second
   turn.
4. `write_stdin` and the exit watcher race to claim completion: exactly one model-context delivery.
5. A bounded `write_stdin` wait is interrupted after subscribing: the process survives and the
   later wake is delivered.
6. Several `f` requests for one process generation merge into one wake.
7. Process-id reuse cannot deliver an old completion to a new process.
8. Explicit terminal stop cancels the pending wake.
9. Thread shutdown cancels pending wake and terminates the process.
10. Fork and cold resume retain delivered audit history but restore no live subscription.
11. Completion output uses existing truncation, redaction, and final-status semantics.
12. Non-paginated and paginated transcript projection render one canonical terminal completion.
13. Active `/goal` continuation does not race a reserved terminal wake.

Tests should use deterministic process gates rather than sleeps where possible and should cover
Linux, macOS, and Windows behavior through the existing unified exec harnesses.

## Suggested implementation sequence

1. Land and validate the `/agent <uuid> <prompt>` TUI work.
2. Define a generic async observation identity and one-shot claim state, reusing agent
   response-observation scheduling where possible.
3. Add `w` parsing and reuse the existing dynamic `write_stdin` schema options.
4. Register terminal observation before the process can cross an unobserved exit boundary.
5. Connect the existing exit watcher to durable observation delivery.
6. Add active-turn injection, idle wake, `/goal`, cancellation, and shutdown handling.
7. Add TUI visibility metadata without duplicating command rows.
8. Add race, persistence, replay, and cross-platform integration coverage.

## Open decisions

- Should `write_stdin(..., w: "f")` return immediately after subscribing when
  `yield_time_ms` is omitted, or preserve its configured wait?
- Should explicit terminal stop create a transcript-only cancellation observation?
- What exact terminal output suffix should a wake carry after earlier tool responses drained
  output?
- Should terminal observation become a generic protocol item shared with future asynchronous tools,
  or remain a unified exec-specific item behind a generic scheduler?
- Should app-server clients receive a distinct terminal-observation notification in addition to
  existing command execution notifications?
