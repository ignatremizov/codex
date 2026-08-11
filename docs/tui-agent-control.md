# TUI user-controlled multi-agent dispatch

Status: proposal

Related work:

- [Multi-agent v1 response observation](multi-agent-v1-response-observation.md)
- [Multi-agent v1 short targets](multi-agent-v1-short-targets.md)
- [TUI chat composer](tui-chat-composer.md)
- [TUI subagent transcript inspection](tui-subagent-transcript-inspection.md)

## Summary

`/agent` should become the user's multi-agent control plane rather than only an agent picker or a
shortcut for sending `turn/start` to an already loaded thread.

The user should be able to:

- prompt an active or idle agent without switching transcripts;
- resume a closed or stored rollout by UUID and prompt it in one operation;
- spawn a new child from a configured role;
- choose whether the source model receives the target's final response passively, wakes for it, or
  does not receive it;
- preserve genuine user-message semantics in the target thread;
- keep every lifecycle action and response visible in durable transcript history.

The displayed source thread is the observer and parent for the operation. This lets the same
command work from Main, from a child coordinating a sibling, or from any other live agent thread.

## Why closed targets should be supported

The initial direct-prompt implementation rejects closed targets. That is a sound boundary for a
narrow `turn/steer` or `turn/start` shortcut: those requests require a loaded thread, and silently
calling generic app-server `thread/resume` would create a live runtime without establishing the
displayed source agent's ownership and response-observation relationship.

That restriction should not become permanent product policy. When a user explicitly names a
closed UUID and supplies a prompt, the request itself authorizes reopening that rollout. A
user-facing multi-agent control operation can resume or adopt the target through the source
thread's `AgentControl`, bind the requested response policy, and then submit the user-authored
prompt.

The implementation must not approximate this by chaining generic TUI `thread/resume` and
`turn/start`. Generic resume is a thread lifecycle operation, not a parent-child control-plane
operation.

## Proposed command grammar

```text
/agent
/agent <target> [w:<mode>] <prompt>
/agent <role> [w:<mode>] <prompt>
/agent +<role> [w:<mode>] <prompt>
```

Examples:

```text
/agent Epicurus Review the latest diff.
/agent 019ff050-d466-73b0-b133-72ecc7c67269 w:f Continue the review.
/agent reviewer w:x Review the API contract.
/agent +reviewer w:f Review the lifecycle races.
```

Bare `/agent` keeps the existing picker behavior.

`+<role>` always spawns. An unprefixed token that uniquely resolves to an existing target addresses
that target. Otherwise, an exact configured role name spawns a new child. If a nickname and role
are ambiguous, the TUI should ask for a UUID or the explicit `+<role>` form.

The optional `w:<mode>` token is recognized only in the option position immediately after the
target or role. A later implementation can support `--` before prompts that intentionally begin
with text resembling an option.

## Target resolution

Existing-target resolution should accept:

1. a canonical full thread UUID;
2. a unique current-session nickname;
3. a unique accepted short thread reference if the short-target proposal is implemented.

UUID remains the durable and auditable identifier. Nicknames and short references are
session-local conveniences and must not be persisted as canonical ownership.

Autocomplete should show:

- active and idle current-session agents;
- closed descendants known to the current agent tree;
- nickname, role, status, and canonical UUID;
- configured roles as visually distinct “new agent” rows.

A manually entered full UUID may identify a stored rollout outside the currently discovered tree.
The server should resolve it read-only before mutation, then resume it only after the complete
command has been accepted. Autocomplete does not need to enumerate every historical rollout.

## Dispatch behavior

### Existing active target

Submit the prompt to the active target turn as genuine user input and bind response handling to
that exact target turn. Preserve the current steering and late-admission race guarantees.

### Existing idle target

Start a target turn with its sticky model, reasoning, permissions, environment, and collaboration
settings. Bind response handling to the returned target-turn identity.

### Closed or stored target

Resume or adopt the target through the displayed source thread's control plane, preserving rollout
identity and available nickname and role metadata. Then start the user-authored turn and bind the
requested response handling.

Resume, prompt admission, and response-observation binding must behave as one lifecycle operation
from the user's perspective. A close, concurrent resume, or target start must not leave a resumed
but unprompted runtime or bind observation to the wrong turn.

### Configured role

Spawn a real child of the displayed source thread with no inherited conversation history by
default. Apply the configured role, nickname selection, model, reasoning, permissions,
environment, depth, and concurrency rules through the same core spawn path used by `spawn_agent`.

The initial user-facing syntax does not need arbitrary model or reasoning overrides. Configured
roles provide the stable customization boundary; explicit overrides can be added later without
changing target resolution.

## Response handling

The user command should reuse the same parsed response-observation policy as model-facing agent
tools:

| Input | Source-model behavior |
| --- | --- |
| omitted | Deliver the final response passively; do not wake an idle source model. |
| `w:f` | Deliver the final response automatically and wake the source model if it is idle. |
| `w:x` | Keep the completion visible in the source transcript without adding this dispatch's final response to source-model context. |

The displayed source thread is always the observer. Switching visual focus after dispatch does not
move the observation to another thread.

The implementation may accept `c`, `cf`, and `cx` through the shared parser for consistency.
However, the primary user needs are passive, wake, and presentation-only final delivery. The TUI
already exposes target commentary directly to the user, so commentary observation should not
complicate the first UI unless it provides clear value to the source model.

### Existing stronger observation

Model-facing response observation is monotonic for one observer and target turn: an earlier or
concurrent `f` wins over a later `x`. Therefore `w:x` on an active turn cannot truthfully promise
presentation-only delivery when the same source already holds a final wake for that turn.

The first implementation should preserve this invariant and explain when a stronger existing
observation remains active. A later explicit user-authoritative observation command may replace or
revoke an existing subscription:

```text
/agent observe <target> passive
/agent observe <target> wake
/agent observe <target> presentation
```

Replacement semantics must be designed separately from per-dispatch `w`; they should not silently
weaken model-authored orchestration without an explicit user action.

## User input and attribution

The target must receive app-server `UserInput`, including text elements, images, skills, plugin
mentions, and connected-app mentions. The command must not fabricate a model-authored
`send_input` tool call or convert structured input to a plain string.

User-authored input and model-authored inter-agent communication may currently share lower-level
turn admission machinery, but their durable and presentation provenance must remain distinct. The
target transcript should render this command as user input. The source transcript should render a
user-initiated agent control action with target, lifecycle action, prompt preview, and response
handling.

## Core and app-server boundary

The TUI should issue a typed app-server v2 request representing user-controlled agent dispatch.
The request needs:

- source thread ID;
- existing-target or spawn-role selector;
- structured `UserInput` items;
- optional response-observation policy.

The app server should locate the live source thread and invoke a shared core lifecycle operation.
The operation should reuse, rather than duplicate, the invariants behind:

- `spawn_agent_with_metadata`;
- closed-rollout resume and live-agent adoption;
- `send_input_observing_response`;
- exact-turn observation binding;
- depth and concurrency checks;
- completion watcher ownership and deduplication.

Model tool handlers and the user-facing request may adapt their different inputs into the same
core operations, but the app server must not invoke a synthetic model tool call. The model did not
author the action, and audit history must not claim that it did.

Any new app-server surface should be v2, typed, schema-generated, and experimental while the
contract is fork-specific.

## Audit and presentation

The source transcript should durably show:

- whether the user sent, resumed-and-sent, or spawned;
- canonical target UUID plus available nickname and role;
- configured role for a new spawn;
- prompt preview;
- requested response handling;
- resulting target status or actionable failure.

The target transcript should show the complete genuine user input. Completion should reuse the
existing observer-relative visible or presentation-only rows and should not duplicate the target's
final answer.

Local TUI info messages are insufficient as the only source-side record because they disappear
from rollout audit history. The core-authored control item needs a stable identity and must project
through live, replay, non-paginated, and paginated transcript paths.

## Lifecycle and race invariants

- Resolve the source, target, and source presentation before mutation.
- Reject self-resume and self-close semantics; a prompt to the currently displayed thread should
  be submitted normally.
- Bind observation to the exact target turn admitted by this dispatch.
- Preserve one final delivery when resume, start, completion, wait, close, or another dispatch
  races.
- Do not create duplicate completion watchers when adopting an already live target.
- Do not resurrect a target while a close is still committing; return an actionable retry result.
- Preserve successful final responses and terminal errors through passive, wake, and
  presentation-only delivery.
- Do not restore live ownership or response subscriptions across process restart, cold resume, or
  fork without a new explicit user command.
- Apply the current source thread's depth and concurrency limits to resume and spawn.
- Keep the displayed source thread usable if target resume, spawn, or prompt admission fails.

## V1 and V2

The user-facing grammar should not expose the selected multi-agent implementation version.
Dispatch should use the source thread's configured V1 or V2 control plane while preserving the
same user-visible behavior:

- genuine user input in the target;
- source-relative response handling;
- durable target identity and metadata;
- exact-once completion presentation;
- closed-target resume when supported by the selected control plane.

Version-specific task names, mailboxes, and watcher implementation remain internal. UUID should be
accepted in both versions even when V2 also offers shorter task-path discovery.

## Future lifecycle commands

The same `/agent` namespace can later expose explicit user actions without overloading prompt text:

```text
/agent interrupt <target>
/agent close <target>
/agent resume <target>
/agent observe <target> <passive|wake|presentation>
```

These commands should share target resolution and durable audit presentation. They are follow-up
work, not required for the initial resume-or-spawn prompt dispatch.

## Required coverage

Integration and TUI coverage should include:

1. Active target receives structured user input through steer without changing source focus.
2. Idle target starts with sticky settings and omitted `w` delivers passively.
3. Closed known descendant resumes, receives the prompt, and retains metadata.
4. Manually entered historical UUID resumes even when absent from current navigation.
5. Missing, archived, closing, and malformed UUIDs return actionable errors without mutation.
6. Exact role spawns with no context by default and `+<role>` resolves ambiguity.
7. Nickname-versus-role ambiguity requires explicit target or spawn syntax.
8. `w:f` wakes the displayed source exactly once.
9. `w:x` produces presentation-only completion on a new turn.
10. Existing `f` remains authoritative when a later active-turn dispatch requests `x`.
11. Source switch after dispatch does not move observation ownership.
12. Child-to-sibling dispatch binds the child as observer.
13. Concurrent close, resume, prompt, completion, and wait races preserve one target turn and one
    final delivery.
14. Source and target transcript items survive replay, compaction, rollback, and pagination.
15. V1 and V2 expose equivalent user-visible behavior.

Tests should use deterministic lifecycle and response gates rather than sleeps.

## Suggested implementation sequence

1. Land the current open-agent direct-prompt and autocomplete slice.
2. Extract shared core user-agent dispatch operations from existing lifecycle handlers without
   changing model-tool behavior.
3. Add the typed app-server v2 request and source-relative audit item.
4. Replace the closed-target error with atomic resume/adopt, prompt, and observation binding.
5. Add role resolution and `+<role>` spawning.
6. Add `w` parsing, autocomplete, presentation, and stronger-existing-observation guidance.
7. Validate V1 first, then preserve the same contract through V2.
8. Add explicit interrupt, close, resume, or observation-replacement commands only as follow-up
   work.

## Open decisions

- Should an unprefixed role always spawn, or only when no current target resolves?
- Should nickname matching be exact, prefix-based, or autocomplete-only?
- Should user `w:x` remain additive like model policy or eventually become an authoritative
  replacement?
- Should `c` modes be user-visible in the first release?
- What durable protocol item best distinguishes a user control action from a model tool call?
- Should historical UUID resume require that the rollout was originally a subagent, or may any
  local rollout be adopted explicitly?
- Should a resumed historical thread retain its old parent metadata for audit while recording a
  new live controller edge?
