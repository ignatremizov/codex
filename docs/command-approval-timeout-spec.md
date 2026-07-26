# Command Approval Timeout

## Summary

Add an optional timeout for command approval requests so `approval_policy = "on-request"` can be
used safely for unattended runs.

When a command approval expires, Codex must reject the command without executing it and return the
rejection to the model so the turn can continue with a safer alternative. Expiration must never be
treated as consent.

This does not change the meaning of `approval_policy = "never"`. That policy continues to suppress
approval prompts and immediately rejects commands that cannot run without approval.

## Motivation

With an unrestricted filesystem sandbox and on-request approvals, ordinary commands can run
without prompting while commands identified as dangerous still require explicit approval:

```toml
sandbox_mode = "danger-full-access"
approval_policy = "on-request"
```

Today, an unanswered command approval can remain pending indefinitely. That makes `on-request`
unsuitable for unattended sessions even though its normal command behavior is otherwise a useful
fit.

Plan-mode `request_user_input` has a superficially similar auto-resolution timer, but its semantics
are different: it submits an empty answer and allows the model to continue using its judgment.
Command approval expiration must instead fail closed by explicitly rejecting the command.

## Proposed Configuration

Add an optional command approval timeout:

```toml
sandbox_mode = "danger-full-access"
approval_policy = "on-request"
approval_timeout_ms = 120000
```

The recommended value is 120 seconds, but configuration accepts any non-negative `u64` millisecond
value. Positive values set a deadline for human responses. Zero rejects command approval requests
that would be routed to a human immediately without emitting a prompt, making `on-request`
equivalent to `never` for human command approvals. Automatic Guardian reviews and permission hooks
are not governed by this setting.

When `approval_timeout_ms` is absent, command approvals retain their current indefinite
wait behavior.

The initial implementation applies this setting only to command approvals. File changes, permission
requests, and other elicitation types remain unchanged. The general name leaves room to apply the
same fail-closed policy to other approval types in later changes, but each additional approval type
must define and test its expiration semantics before adopting the setting.

## Behavior

### Approval policies

- `never`: never prompt. Commands that require approval remain immediately rejected.
- `on-request`: prompt when the existing policy evaluation requires approval. If a command approval
  timeout is configured, reject unanswered command approvals when their deadline expires.
- Other approval policies retain their current command-selection behavior. The timeout may apply to
  any command approval they produce, but it must not change which commands require approval.

The deadline begins only for a user-facing command approval. It does not bound Guardian review
latency and does not apply to commands executed by a reviewer.

This feature does not alter dangerous-command classification. In particular, with
`sandbox_mode = "danger-full-access"` and `approval_policy = "on-request"`:

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

The rejection becomes the tool result, allowing the model to continue the same turn and select a
safer approach.

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

Core owns and enforces the deadline. Clients receive start and expiration timestamps for
presentation and derive their local monotonic fallback timer from the declared duration, but they
are not trusted to enforce core's deadline.

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

The TUI may reuse the visual countdown pattern from `request_user_input`, but it must not reuse that
flow's empty-answer resolution semantics. User interaction with a command approval must not silently
disable its safety deadline.

## Implementation Plan

### 1. Configuration

Update:

- `codex-rs/config/src/config_toml.rs`
- `codex-rs/config/src/profile_toml.rs`
- `codex-rs/core/src/config/mod.rs`
- `codex-rs/core/src/config/config_tests.rs`
- `codex-rs/core/config.schema.json`

Add `approval_timeout_ms: Option<u64>` and carry the effective value into turn configuration
without imposing policy bounds. Run `just write-config-schema` after changing the config types.

### 2. Core timeout enforcement

Update:

- `codex-rs/core/src/session/mod.rs`
- `codex-rs/core/src/state/turn.rs`
- `codex-rs/core/src/tools/approvals.rs`

`Session::request_command_approval` currently registers a one-shot sender, emits an
`ExecApprovalRequestEvent`, and waits for its receiver. Extend this path to race the receiver against
the configured deadline.

On expiration:

1. Atomically remove the matching pending approval.
2. Resolve the request as `ReviewDecision::TimedOut`.
3. Normalize that decision into a clear `ToolError::Rejected`.
4. Leave the command unexecuted.

Pending approval state may need a request identity or generation in addition to the response sender
so timeout cleanup cannot remove a replacement entry with the same external key.

No change should be required to dangerous-command classification in
`codex-rs/core/src/exec_policy.rs`. Existing behavior already maps dangerous commands to approval
under `on-request` and to immediate rejection under `never`.

### 3. Core protocol

Update:

- `codex-rs/protocol/src/approvals.rs`

Add start and optional expiration timestamps to `ExecApprovalRequestEvent`. Their difference
declares the timeout duration without requiring clocks on separate hosts to be synchronized.

The field should remain optional for compatibility with approvals that have no configured timeout
and with older event producers.

### 4. App-server protocol and lifecycle

Update:

- `codex-rs/app-server-protocol/src/protocol/v2/item.rs`
- `codex-rs/app-server/src/bespoke_event_handling.rs`
- `codex-rs/app-server/README.md`
- generated app-server protocol schemas

Expose the optional deadline on `CommandExecutionRequestApprovalParams`. Follow app-server v2
timestamp conventions when choosing the wire name and unit.

When core expires an approval, app-server must resolve or clear its outstanding server request so
clients do not retain an actionable stale approval card. A late client response must not reach an
already expired core request as an effective approval.

Because app-server, the TUI, and the executor may run on different hosts, intermediary timers must
not compare their local wall clock directly with the executor-produced Unix timestamps. Derive the
declared timeout duration from `startedAtMs` and `expiresAtMs`, anchor it to a local monotonic instant
when the request is received, and preserve that instant through deferred or queued handling. Core's
own monotonic deadline remains authoritative.

Delegated approvals must preserve the child request's declared duration instead of applying the
parent's local timeout configuration. MCP approval bridges must likewise cancel their outgoing
elicitation callback and notify the client when the command approval expires.

Run `just write-app-server-schema` after changing the API shape.

### 5. TUI

Update:

- `codex-rs/tui/src/bottom_pane/approval_overlay.rs`
- `codex-rs/tui/src/chatwidget/tool_requests.rs`
- related approval overlay tests and snapshots

Carry the expiration into `ExecApprovalRequest`, schedule redraws while the countdown is visible,
and show fail-closed copy such as:

> Rejects automatically in 1m 42s

When core resolves the request as expired, dismiss the actionable approval or render it as expired.
The TUI countdown is informational; core remains authoritative if the UI is suspended, delayed, or
disconnected.

Because this changes visible UI, add or update `insta` snapshot coverage.

### 6. Documentation

Document:

- the new `approval_timeout_ms` setting and its initial command-approval scope;
- that absence preserves indefinite waiting;
- that expiration rejects rather than approves;
- that `approval_policy = "never"` remains non-interactive;
- app-server deadline and late-response behavior.

## Test Plan

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
- Untimed approvals omit the deadline.
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
- Adding timeouts to file-change, MCP, permission, or other non-command approvals in the initial
  implementation.
- Changing which commands the built-in dangerous-command heuristic recognizes.

## Open Questions

1. Should the TUI offer an explicit "keep waiting" action? If added, it must request a new deadline
   from core rather than locally disabling expiration.
2. Which existing app-server resolved-request notification should represent core-owned expiration,
   or is an explicit expiration reason needed on that notification?
