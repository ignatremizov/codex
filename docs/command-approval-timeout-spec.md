# Command Approval Timeout

## Summary

Add an optional timeout for command approval requests so `approval_policy = "on-request"` can be used safely for unattended runs.

When a command approval expires, Codex must reject the command without executing it and return the rejection to the model so the turn can continue with a safer alternative. Expiration must never be treated as consent.

This does not change the meaning of `approval_policy = "never"`. That policy continues to suppress approval prompts and immediately rejects commands that cannot run without approval.

## Motivation

With an unrestricted filesystem sandbox and on-request approvals, ordinary commands can run without prompting while commands identified as dangerous still require explicit approval:

```toml
sandbox_mode = "danger-full-access"
approval_policy = "on-request"
```

Today, an unanswered command approval can remain pending indefinitely. That makes `on-request` unsuitable for unattended sessions even though its normal command behavior is otherwise a useful fit.

Plan-mode `request_user_input` has a superficially similar auto-resolution timer, but its semantics are different: it submits an empty answer and allows the model to continue using its judgment. Command approval expiration must instead fail closed by explicitly rejecting the command.

## Configuration

Add an optional command approval timeout:

```toml
sandbox_mode = "danger-full-access"
approval_policy = "on-request"
approval_timeout_ms = 120000
```

The recommended value is 120 seconds, but configuration accepts any non-negative `u64` millisecond value. Positive values set a deadline for human responses. Zero rejects command approval requests that would be routed to a human immediately without emitting a prompt, making `on-request` equivalent to `never` for human command approvals. Automatic Guardian reviews and permission hooks are not governed by this setting.

When `approval_timeout_ms` is absent, command approvals retain their current indefinite wait behavior.

The initial implementation applies this setting only to command approvals. File changes, permission requests, and other elicitation types remain unchanged. The general name leaves room to apply the same fail-closed policy to other approval types in later changes, but each additional approval type must define and test its expiration semantics before adopting the setting.

## Behavior

### Approval policies

- `never`: never prompt. Commands that require approval remain immediately rejected.
- `on-request`: prompt when the existing policy evaluation requires approval. If a command approval timeout is configured, reject unanswered command approvals when their deadline expires.
- Other approval policies retain their current command-selection behavior. The timeout may apply to any command approval they produce, but it must not change which commands require approval.

The deadline begins only for a user-facing command approval. It does not bound Guardian review latency and does not apply to commands executed by a reviewer.

This feature does not alter dangerous-command classification. In particular, with `sandbox_mode = "danger-full-access"` and `approval_policy = "on-request"`:

- ordinary unmatched commands run without prompting;
- commands matched by the built-in dangerous-command heuristic prompt;
- exec-policy rules with `decision = "prompt"` prompt;
- exec-policy rules with `decision = "forbidden"` remain immediately forbidden.

### Resolution

Once a command approval is emitted, exactly one of these outcomes must win:

1. Explicit approval before the deadline executes the command.
2. Explicit rejection before the deadline rejects the command.
3. Turn interruption aborts the pending command.
4. Deadline expiration rejects the command without executing it.

Expiration should return a distinct rejection to the model, for example:

> Command approval expired after 120 seconds. The command was not executed. Use a safer approach.

The rejection becomes the tool result, allowing the model to continue the same turn and select a safer approach.

### Safety and race requirements

- The command must not begin execution until an approving decision wins.
- Expiration must be equivalent to an explicit denial for execution safety.
- A response received after expiration must be ignored.
- A response racing with expiration must have exactly one winner.
- Expiring one request must not remove or resolve a newer request that reused the same key.
- Interrupting or completing a turn must cancel pending expiration work.
- Explicit exec-policy `forbidden` decisions must never become approvable.
- Client disconnection must not disable the core-owned deadline.

## Architecture

Core owns and enforces the deadline. Clients receive start and expiration timestamps for presentation and derive their local monotonic fallback timer from the declared duration, but they are not trusted to enforce core's deadline.

```text
policy evaluation
       |
       v
command needs approval
       |
       v
core registers pending approval and deadline
       |
       +-------------------+-------------------+
       |                   |                   |
       v                   v                   v
approve/reject         turn interrupt       deadline
       |                   |                   |
       v                   v                   v
resolve once          abort pending       timed-out denial
       |
       v
execute only if explicitly approved
```

The TUI may reuse the visual countdown pattern from `request_user_input`, but it must not reuse that flow's empty-answer resolution semantics. User interaction with a command approval must not silently disable its safety deadline.

## Implemented architecture

Core enforces the monotonic deadline through a private receipt-bearing submission envelope. Each approval keeps its exact generation and originating turn; fresh opaque `approval_id` values identify callbacks while `call_id` remains command provenance. Root presentation ownership is private and provenance-based, not inferred from callback-ID presence. RAII-style claims and serialized enqueue preserve fail-closed admission and 0bc rollback quarantine.

Adapters derive a local monotonic deadline from optional start/expiry timestamps and preserve the receipt through buffering. Connection callbacks capture the receipt before lookup/removal while retaining connection ownership, authentication identity, revision, cancellation, and payload-safe error logging. TUI countdowns are informational; core remains authoritative.

### Configuration and core

The configuration sources are `codex-rs/config/src/config_toml.rs` and `codex-rs/config/src/profile_toml.rs`; the effective value is materialized through the existing profile layer before `codex-rs/core/src/config/mod.rs` builds runtime configuration. The value is `Option<u64>`: absent is indefinite, zero rejects before prompting, and positive values define the deadline. The internal pending state is owned by `codex-rs/core/src/state/turn.rs`.

Command approval is implemented in the focused `codex-rs/core/src/session/command_approval.rs` and `codex-rs/core/src/session/request_command_approval.rs` modules. Exact-request claims and the private receipt-bearing submission envelope prevent replacement, stale, or late decisions from changing execution. Dangerous-command classification and explicit forbidden rules are unchanged.

### Protocol and app-server adapter

`ExecApprovalRequestEvent` carries an internal required start time and an optional expiry. The public command-approval request emits nullable timestamps and accepts omitted timing from older servers. Incomplete timing means an untimed approval. Each adapter derives the duration from the request timestamps and anchors it to a local monotonic receipt. Separately, `app-server/src/outgoing_message.rs` timestamps client responses before callback lookup/removal.

The adapter preserves connection ownership, authentication identity and revision checks, synthetic cancellation, request-resolution ordering, and payload-safe error logging. A late response cannot become an effective approval after core expiration. Schema regeneration and remote protocol validation remain deferred.

### TUI presentation

The TUI carries the receipt through `tui/src/approval_events.rs`, `tui/src/bottom_pane/approval_overlay.rs`, and deferred approval queues. It schedules redraws while the countdown is visible and shows fail-closed copy such as:

> Rejects automatically in 1m 42s

When core resolves the request as expired, the TUI dismisses or disables the actionable approval. The countdown is informational and is not reset by interaction, buffering, replay, suspension, or disconnection.

Because this changes visible UI, add or update `insta` snapshot coverage.

### Documentation contract

Document:

- the new `approval_timeout_ms` setting and its initial command-approval scope;
- that absence preserves indefinite waiting;
- that expiration rejects rather than approves;
- that `approval_policy = "never"` remains non-interactive;
- app-server deadline and late-response behavior.

## Validation coverage

The following cases define required coverage; they are not a claim that local tests or remote schema generation have been run. Executable validation, snapshots, and generated protocol artifacts remain outstanding remote/CI work.

### Policy and configuration tests

- Timeout values, including zero and large positive values, load from global configuration.
- Timeout values load from a named profile.
- Zero rejects command approval requests without emitting a prompt.
- An absent value preserves existing behavior.
- `never` still rejects dangerous commands immediately without prompting.
- `on-request` still prompts for dangerous commands.
- Explicit `forbidden` rules remain immediately forbidden.
- Ordinary commands under `danger-full-access` and `on-request` still run without prompting.

### Core integration tests

- Approval before the deadline executes the command.
- Explicit rejection before the deadline does not execute the command.
- No response causes a timed-out rejection and does not execute the command.
- The model receives the timeout rejection and can issue a later, safer tool call.
- A response racing with expiration resolves the request exactly once.
- Approval after expiration is ignored.
- Turn interruption clears the approval and its deadline.
- A later approval using the same external key is not removed by stale timeout cleanup.
- An approval without a configured timeout remains pending until resolved or interrupted.

Use paused Tokio time where practical so timeout tests are deterministic and fast.

### App-server tests

- Command approval params include the configured absolute deadline.
- Untimed approvals emit a null deadline; older servers may omit it.
- Expiration resolves the outstanding server request.
- A late approval response is ignored or reported as stale and cannot execute the command.
- Schema fixtures include the optional deadline.

### TUI tests

- Timed command approvals render a countdown.
- Untimed command approvals retain the existing presentation.
- Countdown text clearly communicates automatic rejection.
- Expiration removes or disables the actionable approval.
- User interaction does not silently disable the deadline.
- Snapshot coverage captures the timed and expired states.

## Non-goals

- Changing plan-mode `request_user_input` timeout behavior.
- Treating silence as approval.
- Making `approval_policy = "never"` interactive.
- Making explicit `forbidden` exec-policy rules overridable.
- Adding timeouts to file-change, permission, or other non-command approvals.
- Changing which commands the built-in dangerous-command heuristic recognizes.
