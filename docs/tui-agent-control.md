# TUI user-controlled multi-agent dispatch

Related work:

- [Multi-agent v1 response observation](multi-agent-v1-response-observation.md)
- [Multi-agent v1 short targets](multi-agent-v1-short-targets.md)
- [TUI chat composer](tui-chat-composer.md)
- [TUI local app-server handoff and fallback](tui-local-daemon-handoff.md)
- [TUI subagent transcript inspection](tui-subagent-transcript-inspection.md)

## Summary

`/agent` should be the user's native multi-agent control plane: a responsive pane plus compact
commands for dispatch, observation, lifecycle control, and transcript inspection.

This is the Codex-native successor to the separate Supervisor TUI concept. That application was
designed before Codex exposed multi-agent lifecycle tools, durable agent transcripts, response
observation, configured roles, and integrated approvals. Keeping orchestration inside the Codex
TUI removes a second process, protocol cache, transcript model, approval queue, reconnect loop, and
logging system while giving the user direct access to the richer native thread graph.

The user should be able to:

- prompt an active or idle agent without switching transcripts;
- resume a known closed descendant by UUID and prompt it in one operation, or give explicit
  `resume` a stored rollout UUID to load its history and adopt it as a child;
- spawn a default child or a child from a configured role;
- choose whether a new child receives no parent context, all effective parent context, or the last
  N parent turns;
- choose whether the source model receives the target's final response passively, wakes for it, or
  does not receive it;
- deliberately grant one target turn an attributed route back to the source model;
- queue a complete prompt and its response policy as a distinct future target turn;
- preserve genuine user-message semantics in the target thread;
- keep every lifecycle action and response visible in durable transcript history.

The displayed source thread authors and observes the operation. It becomes the lifecycle parent
when spawning a new child or explicitly adopting a stored target outside its current root.
Same-root existing targets retain their graph parent. This lets the same command work from Main,
from a child coordinating a sibling, or from any other live agent thread.

## Consolidating the earlier standalone supervisor design

The earlier standalone supervisor design remains useful as an operator-UX inventory. Its features
should map onto native Codex state rather than being reimplemented in another application:

| Supervisor surface | Codex-native form |
| --- | --- |
| Persistent agent list | `/agent` control pane backed by the native agent graph. |
| Overview and Inspect panes | Responsive `/agent` overlay using canonical thread transcripts. |
| `<id|name> <prompt>` | `/agent <target> [w:<w-mode>] <prompt>`. |
| Preconfigured `--agent` entries | Configured reusable roles. |
| `pending` and `start` | Not carried over; native prompt dispatch starts or steers immediately. |
| Prompt queue for busy agents | `/agent queue <target> ...`, mirroring Tab for the selected thread. |
| `stop` | Separate turn interruption and agent closure actions. |
| `list`, `show`, and `threads` | Bare `/agent`, prompt-less target selection, and stored-UUID lookup. |
| `dump` | Existing rollout storage and any general transcript export outside `/agent`. |
| Per-agent approvals | Existing parent-TUI approval flow and inactive-thread status indicator. |
| Review commands | Configured reviewer roles or ordinary prompts; built-in `/review` remains separate. |
| WAIT_FOR dependency gates | Model-facing response observation and wait tools; no second user scheduler. |
| `--max-parallel` | Existing multi-agent concurrency configuration and admission checks. |
| Worktree provisioning | Optional future environment template, not part of agent dispatch itself. |
| App-server supervision and reconnect | Existing Codex harness lifecycle. |
| Rotated Supervisor logs | Existing rollouts, state database, transcript export, and retention policy. |

The native control pane should not copy Supervisor's always-visible two-pane layout. Codex is
primarily a focused conversation TUI, so agent management belongs in an Overview overlay whose
contextual Inspect action opens the canonical transcript pager and then returns to the exact prior
pane and transcript position.

## Goals

- Make common multi-agent orchestration possible without spending a model turn on administrative
  tool calls.
- Keep model-authored and user-authored lifecycle actions distinct and auditable.
- Reuse one canonical agent graph, transcript, approval path, and lifecycle engine.
- Support direct supervision from Main and peer coordination from any live child.
- Make agent state, pending approval status, response handling, and failures discoverable in one
  pane.
- Preserve compact command entry for experienced users while providing autocomplete and visible
  actions for discovery.

## Non-goals

- Building a second general workflow engine beside native agent lifecycle.
- Maintaining duplicate per-agent transcript or log buffers.
- Spawning or supervising another app-server process.
- Replacing the existing focused transcript as the primary Codex UI.
- Automatically restoring live agents or response subscriptions after a cold restart without
  explicit user confirmation.

## Why closed targets should be supported

Closed targets require a control-plane resume or adoption operation, not merely `turn/steer` or
`turn/start`. Those turn requests require a loaded thread, while generic app-server
`thread/resume` would create a live runtime without establishing the displayed source agent's
ownership and response-observation relationship.

When a user explicitly names a known closed descendant and supplies a prompt, the request
authorizes reopening that rollout. A user-facing multi-agent control operation should resume the
target through the source thread's `AgentControl`, bind the requested response policy, and then
submit the user-authored prompt.

A UUID identifies a rollout, which may itself be another root's Main, a descendant in another
root, or a standalone rollout with no alias namespace. Supplying that UUID to
`/agent resume <uuid>` explicitly authorizes loading the rollout with its existing history and
adopting it as a child of the displayed source. Ordinary prompt dispatch does not implicitly
reparent an out-of-root rollout; after resume establishes the control relationship, the user or
model can prompt it normally. A runtime that is still live under another root must first be closed
so two roots cannot control it simultaneously.

The TUI must not approximate this by independently chaining `thread/resume` and `turn/start`.
User commands, model tools, and generic thread resume must converge on one ownership-aware Core
resume operation. Source-relative calls may preserve or transfer a control relationship before
admitting input; a standalone `codex resume` with no source relationship reopens the rollout under
its current durable owner, or under its persisted root identity when it has no owner.

## Proposed command grammar

```text
/agent
/agent new [fork:<none|all|N>] [w:<w-mode>] [<prompt>]
/agent <target>
/agent <role> [fork:<none|all|N>] [w:<w-mode>] [<prompt>]
/agent <target> [w:<w-mode>] <prompt>
/agent queue <target>
/agent queue <target> [w:<w-mode>] <prompt>
/agent interrupt <target>
/agent interrupt <target> [w:<w-mode>] <follow-up prompt>
/agent close <target> [w:<w-mode>]
/agent <target> close [w:<w-mode>]
/agent resume <target> [w:<w-mode>] [<prompt>]
/agent observe <target> <passive|wake|presentation>
```

Selectors accept compact unprefixed forms and explicit namespaces:

```text
<target> = <uuid> | <decimal-ref> | <nickname>
         | id:<uuid> | ref:<decimal-ref> | nick:<name>
<role>   = <ordinary-role-name> | role:<name>
<w-mode> = one or more unique flags in cfmqx order
```

Examples:

```text
/agent Epicurus
/agent Epicurus Review the latest diff.
/agent 2 w:f Continue the review.
/agent ref:2 w:f Continue the review.
/agent nick:"Ada Lovelace" Review the latest diff.
/agent 019ff050-d466-73b0-b133-72ecc7c67269 w:f Continue the review.
/agent new w:x
/agent new fork:all w:x
/agent reviewer w:f
/agent role:"2" w:f
/agent reviewer fork:3 w:x Review the API contract.
/agent queue Epicurus w:f After that, check the test coverage.
/agent Epicurus w:fq After that, check the test coverage.
/agent Epicurus w:fm Review the contract and ask Main about ambiguous requirements.
```

Bare `/agent` opens the control pane. `/agent <target>` opens it with that existing agent selected
without starting or steering a turn.

`/agent new` selects a default child spawn without requiring a configured role. Its prompt-less
and prompted forms otherwise follow the same focus and response-observation behavior as a
configured role spawn.

An unprefixed exact configured role name selects a new-child spawn, including when one or more
agents already use that role. Without a prompt, it creates a real idle child and switches into
that child's blank transcript and composer. If a nickname equals an ordinary configured role
name, the role meaning wins; use the agent's ref, UUID, or `nick:` selector to target the existing
agent.

`fork:<mode>` is valid only for default or configured-role spawns. `w:<w-mode>` is valid wherever
the grammar shows it. Option tokens appear after the selector, may be given in either order, and
may each occur at most once.

The control prefix uses these lexical rules:

- Unquoted whitespace separates control tokens. Double quotes group selector text; quote
  delimiters are removed, `\"` and `\\` are decoded, and an unmatched quote is an error.
- Quoting may follow a namespace prefix, so `nick:"Ada Lovelace"` is one selector whose value is
  `Ada Lovelace`.
- After the action or selector, the parser consumes recognized `fork:` and `w:` options until the
  first non-option token. The untouched input beginning at that token is the prompt.
- `--` ends option parsing and is removed; the untouched text after it is the prompt. Use it when
  a prompt intentionally begins with `fork:` or `w:`.
- Attached images count as structured prompt input even when no prompt text follows the control
  tokens. They start or queue work anywhere the grammar accepts a prompt.
- Unknown values for the recognized `fork:` and `w:` prefixes, misplaced recognized options, and
  duplicate options fail before any lifecycle mutation. Other `name:value` text begins the prompt.

Autocomplete inserts the forced form when a nickname or role is numeric, UUID-shaped,
action-shaped, option-shaped, contains whitespace, or otherwise collides. A bare generated
nickname normally needs no prefix.

Action words such as `new`, `queue`, `interrupt`, `close`, `resume`, and `observe` are reserved in
command entry. A colliding configured role uses `role:<name>` and a colliding existing agent uses
`nick:<name>`, its numeric ref, or its UUID.
`Main` is also a reserved nickname in every case variation; a configured role
named `main` therefore uses `role:main`.

## Target resolution

Existing-target resolution should accept:

1. a canonical full thread UUID;
2. a persisted root-scoped decimal ref under the short-target contract;
3. an exact persisted nickname, plus the reserved case-insensitive `Main` nickname.

UUID remains the canonical and externally auditable identifier. Refs and nicknames are persisted
root-scoped aliases that survive cold resume without replacing UUID ownership.

The complete command precedence is:

1. a reserved action immediately after `/agent`;
2. `new`;
3. forced `id:`, `ref:`, `nick:`, or `role:` selectors;
4. an unprefixed canonical UUID;
5. an unprefixed canonical decimal ref;
6. the reserved unprefixed `Main` nickname in any case, which targets;
7. an unprefixed exact configured role, which spawns;
8. an unprefixed exact ordinary nickname, which targets;
9. an actionable unknown/ambiguous-selector error.

This means numeric- or UUID-shaped roles require `role:`, while similarly
shaped nicknames require `nick:`. Fuzzy and prefix matching are autocomplete
aids only and never execute directly.

Autocomplete should show:

- active and idle current-session agents;
- closed descendants known to the current agent tree;
- stable numeric ref, nickname, role, status, and canonical UUID;
- reserved action verbs in the first argument, then only existing agents in an action's target
  argument;
- `passive`, `wake`, and `presentation` after an `observe` target;
- a visually distinct “new default agent” row;
- configured roles as visually distinct “new agent” rows.

Main owns ref `1` and nickname `Main`; descendants receive monotonic refs in root-wide
spawn/adoption order. The
control pane must render these stored refs instead of the generic selection widget's filtered row
numbers. Filtering, closing agents, or inserting role rows must not renumber them, and refs are
never reused within the root lineage. Every thread in the root resolves the same map, allowing a
child to address Main as `main`, `Main`, or `1`, and a sibling by its displayed ref.

A manually entered full UUID may identify a stored rollout outside the currently discovered tree.
The server may resolve it read-only for inspection. Mutation requires an explicit resume/adopt
operation after the complete command has been accepted. Autocomplete does not need to enumerate
every historical rollout.

Refs and nicknames resolve only in the displayed source's root namespace. An explicit UUID outside
that namespace may identify another root's Main or one of its descendants and may adopt the
resumable local rollout, but adoption transfers exclusive ownership; it does not add a second
controller. Preserve the rollout's historical source and parent metadata, tombstone old aliases,
revoke old live subscriptions, replace the current control edge, allocate the new root's aliases,
and record the transfer before publishing the resumed target. Another root's Main receives a
normal generated child nickname at this resume boundary; its reserved `Main` nickname remains
local to the old root namespace.

The detailed storage, backfill, fork, and transfer transaction is defined by
[Multi-agent V1 short targets](multi-agent-v1-short-targets.md). The TUI must consume that
committed state rather than infer refs from list order.

## Operation authority

Selector resolution does not itself authorize an operation:

| Action | Current-root target | UUID outside current root |
| --- | --- | --- |
| inspect | Allowed. | Read-only lookup may be allowed. |
| prompt, queue, wait/observe, interrupt, close | Allowed when operation-specific lifecycle checks pass. | Reject as not controlled. |
| resume closed descendant | Resume within the current root. | Not applicable. |
| explicit resume/adopt | Idempotent when already controlled. | Validate and transfer exclusive ownership. |

Self-resume, self-close, and self-observe remain invalid. Main may receive
same-root input and observation, but a child cannot close Main. User-authored
commands have direct user authority within the selected root. Knowledge of an
unrelated UUID authorizes cross-root mutation only through explicit
`/agent resume`; every other mutation remains limited to the selected root.

## Resume ownership, liveness, and depth

Resume behavior is graph-relative, not caller-type-relative. The user-facing command, V1 model
tool, V2 adapter, app-server request, and ordinary thread-resume entry point must use the same Core
ownership decision:

- A target already owned by the caller's root is reopened under that owner. Its persisted parent
  and depth remain unchanged even when a parent, child, or sibling initiated the resume.
- A stored target outside the caller's root is adopted beneath the caller only when the caller
  explicitly supplies a source-relative resume operation. Adoption transfers the selected target
  and its persisted subtree to the caller's root.
- A standalone resume with no caller root reopens the target under its current durable owner. An
  unowned standalone rollout retains its persisted root identity.
- A target or persisted descendant that is live under another root cannot be adopted. The caller
  must close that live subtree first.

Durable alias ownership and runtime liveness are deliberately separate. The alias store records
which root exclusively controls a rollout; `active` means live or resumable and is not a process
heartbeat. Within one app-server, the shared thread manager's loaded-thread map provides exact
runtime liveness across every root. Across app-server processes sharing one thread store, the
store's exclusive live-writer lease is authoritative. Resume must acquire that lease before
transferring durable ownership. A competing writer therefore fails without changing aliases, and
the operating system releases a local writer lock after process exit without requiring process
enumeration or a stale database heartbeat.

`agent_max_depth` limits autonomous model-created graph edges. It does not reject an explicit user
`/agent new`, role spawn, or `/agent resume` operation. Every user-created or adopted edge still
records its actual depth, and concurrency limits still apply. A V1 model at or beyond the
configured maximum keeps `send_input` so it can communicate with Main and same-root peers, but it
does not receive spawn, resume, wait, or close tools. User `/agent` controls remain available from
that thread. If deeper autonomous orchestration is needed later, it should require an explicit
session-local capability/depth promotion; deferred tool discovery alone must not grant authority.

## Control pane

Bare `/agent` opens a responsive control pane.

At wide terminal widths:

- the left side shows the native agent tree in stable spawn order;
- the right side shows Overview for the selected entry;
- a compact action row exposes prompt, queue follow-up, interrupt, resume, close, observe, and
  transcript inspection or navigation when applicable.

At narrow widths, use a list-first flow that opens the selected detail view full-width. Returning
must restore selection, scroll position, filter text, and the transcript position from which the
pane was opened.

Enter switches to the selected canonical transcript. Tab opens contextual controls for that row,
including a non-switching transcript inspection overlay;
prompt, queue, interrupt-with-follow-up, resume, observe, and close controls prepare the equivalent
auditable `/agent` command in the source thread's composer for confirmation. This preserves any
existing draft and attachments instead of executing a destructive action from an accidental key
press.

### Agent rows

Each row should show concise text labels in addition to styling:

- stable numeric ref, nickname, role, and canonical UUID detail;
- starting, running, waiting, idle, completed, interrupted, closed, or errored state;
- active response-observation marker relative to the displayed source;
- queued follow-up and pending approval indicators;
- parent/child structure without flattening canonical UUID ownership.

The status model may project several native facts into one display state, but it must not invent a
second authoritative lifecycle state. For example, “approval pending” decorates a running turn;
“waiting” may describe an active tool wait.

### Overview

Overview summarizes all agents without copying their histories:

- current task or latest user prompt preview;
- latest complete commentary or final-response preview;
- model and reasoning effort;
- requested fork mode for a newly spawned child;
- elapsed running or waiting time;
- queued follow-up previews;
- pending approval state;
- child count and terminal outcome.

### Inspect

Inspect loads the selected canonical thread transcript through the existing transcript reader and
fixed-Full pager overlay. It preserves structured commands, patches, assistant phases, completion
visibility, and non-paginated or paginated history behavior without maintaining a separate
last-20-lines buffer.

Enter switches the primary TUI view to a selected live controlled thread. For a closed,
transferred, or out-of-root read-only row, Enter inspects the canonical transcript instead of
implicitly loading or adopting it. Back from transcript inspection returns to the control pane,
while Back from a full thread switch follows the existing parent navigation contract.

### Actions and accessibility

Every action needs a discoverable text label and keyboard path; color alone cannot carry status or
response visibility. Destructive close and interrupt actions must be distinct. Disabled actions
should explain whether the target is current, closed, starting, closing, unavailable, or blocked
by concurrency, ownership, or liveness.

In the selected-agent prompt composer, Enter dispatches immediately. `/agent queue` explicitly
adds a distinct turn to the shared target-owned agent FIFO. Tab retains the ordinary composer queue
for the currently displayed thread; it is not a second implementation of cross-agent queued work.
Queued agent turns are selectable in the agent control pane and support explicit edit and removal.

The new-child composer exposes `none`, `all`, and positive last-N fork modes, defaulting to
`none`, alongside the response-observation choice.

## Dispatch behavior

### Existing active target

Submit the prompt to the active target turn as genuine user input and bind response handling to
that exact target turn. Preserve the current steering and late-admission race guarantees.

### Existing idle target

Start a target turn with its sticky model, reasoning, permissions, environment, and collaboration
settings. Bind response handling to the returned target-turn identity.

### Closed or stored target

Prompting a known current-root closed descendant resumes it through its existing control
relationship. Directly prompting an out-of-root UUID does not implicitly reparent it and directs
the user to `/agent resume <uuid>` first. The selected rollout may itself be another root's Main.
That explicit command loads the stored rollout with its existing history and requests exclusive
adoption into the displayed source's root. Adoption uses the durable transfer transaction from the
short-target contract: it validates the rollout and complete persisted subtree, tombstones the old
aliases, revokes old response subscriptions, replaces the target's current control edge, allocates
aliases for the subtree in the new root, and records the transfer before the target is published to
the new controller. Descendant edges remain attached. Historical rollout source, history, and
parent metadata remain unchanged.

After ownership is established, start the user-authored turn and bind the requested response
handling. A target that is closing, has any live persisted descendant under another controller,
is archived without an allowed unarchive transition, or is otherwise unable to transfer
atomically returns an actionable error instead of acquiring a second controller.

Resume, prompt admission, and response-observation binding must behave as one lifecycle operation
from the user's perspective. A close, concurrent resume, or target start must not leave a resumed
but unprompted runtime or bind observation to the wrong turn. The persisted operation may use an
intermediate adopting/starting state, but failures must either leave that state safely resumable or
restore the prior exclusive owner; they must never expose simultaneous ownership.

### Standalone resume

`/agent resume <target> [w:<w-mode>] [<prompt>]` reopens or adopts a target. Without a prompt it
establishes a live control relationship and reserves the response policy for the target's next
turn. With a prompt it admits that first turn immediately under the reserved policy. Repeating it
for an already controlled live target is idempotent.

For a stored UUID outside the current root, including a UUID that identifies another root's Main,
the command resumes the original rollout—not a fresh fork—so its canonical UUID and conversation
history remain available while it becomes a subagent of the displayed source. The command first
acquires exclusive live-writer ownership for the stored rollout. It may construct an unpublished
runtime behind that lease, but it commits the exclusive alias/subtree transfer before making that
runtime discoverable or accepting input. For a current-root closed descendant, it retains the
existing alias and parent edge. Two concurrent adoption attempts must have one committed winner;
the loser receives the current owner in an actionable conflict result. A writer conflict leaves
the prior durable owner unchanged.

If exclusive ownership commits but later runtime or response-observation setup fails, return a
committed resume outcome with an explicit warning. The new root remains the sole owner, the target
stays safely resumable, and the TUI must not show the requested observation as installed.

The response policy applies once to the target's next admitted turn and then expires. It is not
restored after process restart, cold resume, or fork.

The resume response reports whether observation bound the active turn, an undelivered completion,
or the next turn. Clients must use that authoritative binding instead of inferring it from a later
liveness read, because completion can race the read.

### Close response replay

`/agent close <target> [w:<w-mode>]` and `/agent <target> close [w:<w-mode>]` are equivalent. They
end the target runtime and any open descendants. When the target already completed, Core replays
its exact final response only if that response is absent from the source's effective model
history. A structured `wait_agent` result or canonical background-delivery envelope therefore
suppresses duplication while it remains in context. Prose mentions and compaction summaries are
not delivery receipts, so a compaction/replacement boundary permits close to restore the exact
response.

Omitted `w` delivers that replay passively. `f` wakes an idle source, `q` queues the replay for the
source's next turn, and `x` keeps it out of source-model context. `c` and `m` have no retrospective
effect because close starts no target turn. Closing Running, Interrupted, Shutdown, or NotFound
state has no final response to replay. Replayed model context keeps the source session's native V1
or V2 completion envelope. Replay is process-lifetime state and is not reconstructed if the
process stops before the source consumes it; the closed target rollout can still be resumed
explicitly.

Client transcript projection renders a replay through the same canonical completion cell as an
ordinary observed completion: model-context delivery is labeled `● visible`, while `x` is labeled
`○ not visible`. The internal V1 `<subagent_notification>` envelope remains in model context and
the rollout for auditability but must not render as an ordinary
`Agent message from /root/thread_*` row.

### New child

`/agent new` spawns a child using the normal default subagent settings. `/agent <role>` applies the
selected configured role, including its instructions, model, reasoning, permissions, environment,
and other settings. Both forms create a real child of the displayed source thread with no
inherited conversation history by default and use the same core spawn path as `spawn_agent`.

With a prompt, start the child's first turn immediately and leave visual focus on the source.
Without a prompt, create the child in an idle state, establish the source as its parent and
observer, and switch visual focus into the child so the user can author its first input directly.
This resembles opening a new Codex session whose instructions and settings come from the default
subagent configuration or selected role, while retaining native subagent identity and lifecycle
ownership.

The prompt-less form allocates a real rollout and agent slot immediately, so normal concurrency
checks apply. It records the resulting graph depth, but explicit user spawning is not rejected by
the autonomous model depth budget. Closing the unused child releases its live slot.

The prompt-less form's response policy is a one-shot reservation for the first target turn. Omitted
`w` gives that turn normal passive delivery to the parent; `w:f` wakes the parent on completion;
and `w:x` keeps the completion presentation-only. Commentary combinations behave like the
model-facing tools. The reservation expires after that first turn and does not observe later user
turns in the child unless another dispatch, resume, or observe action explicitly does so.

This also provides an isolated side-conversation flow without loading `/side`. `/agent new w:x`
uses the default child configuration, while `/agent <role> w:x` uses a configured role. Both open
the child for direct user interaction while keeping its first task and completion out of the
parent's model context. The parent transcript retains the presentation and graph audit trail.

### Forked context

User-spawned default and configured-role children accept one version-neutral fork option:

| Input | Child initial model context |
| --- | --- |
| omitted or `fork:none` | No parent conversation history. |
| `fork:all` | All effective parent model context available at spawn. |
| `fork:N` | The last positive `N` fork turns from the parent. |

`fork:none` is the default for user-controlled spawning in both V1 and V2. `fork:0`, negative
values, booleans, and other strings are rejected. `all` and `none` are clearer than `true` and
`false`, map directly to the shared fork modes, and avoid maintaining aliases in a new interface.

`fork:all` means effective model context, not the raw pre-compaction rollout. It respects rollback
and compaction: older material may be represented by the parent's effective compacted context.
`fork:N` uses the existing V2 fork-turn definition and truncation path, including real user-turn
and trigger-turn inter-agent boundaries, rollback filtering, and removal of stale startup context.
Source-relative agent-task, commentary, and completion-observation markers are removed from both
ordinary and compacted inherited history because the child does not inherit those live observer
relationships.

The fork snapshot is taken when the child is created. For a prompt-less idle child, parent work
performed after creation does not appear in the child's eventual first turn.

The TUI parses all three modes into the shared core `SpawnAgentForkMode`. V1 user dispatch can
therefore use numeric `LastNTurns` without adding `fork_turns` to the model-visible V1
`spawn_agent` tool. V1 model tools retain `fork_context: bool`; V2 model tools retain
`fork_turns`. The user-facing `/agent` contract does not vary with the configured multi-agent
version.

This `fork:` option controls only the new child's initial model context. The child remains in the
displayed source's root alias namespace and receives the next ref from that namespace.

A separate root-thread fork, such as `codex fork`, creates a new alias namespace and carries no
live ownership or response subscriptions. A no-history root fork starts with Main `1`/`Main` and
next ref `2`. A history-bearing root fork also binds only its new Main to `1`/`Main`; old child
refs and nicknames remain unknown, while the source numeric high-water mark and child nickname set
are imported as reservations so inherited history cannot silently target newly spawned agents.
Explicit UUID adoption into that root allocates fresh aliases.

The base user-facing syntax does not need arbitrary model or reasoning overrides. Configured roles
provide the stable customization boundary; explicit overrides can be added later without changing
target resolution.

### Queued follow-up

`/agent queue <target> [w:<w-mode>] <prompt>` queues a distinct future target turn.
`/agent <target> w:q <prompt>` is its compact equivalent. If the target has an active or pending
turn, append the complete structured user input and the remaining `w` policy to the shared
process-lifetime, target-owned FIFO instead of steering the active turn. Queued follow-ups start
one turn at a time in FIFO order.

The target turn's final response uses the source's ordinary next-turn input queue. It never steers
source work that is still active: it starts immediately when the source is idle, or after the
current source turn ends. This applies to omitted/passive final handling as well as `f`; `x` keeps
the response presentation-only.

If the target is idle or closed by the time the queue operation is admitted, dispatch the prompt
immediately through the normal start or resume-and-start path. This includes the race where the
active turn completes while the command is being resolved. `queue` accepts only an existing
target; configured role names continue to use ordinary prompt syntax and spawn immediately.

The optional response-observation and `m` policy belongs to the future turn started by that queued
input, not the turn that was active when the user queued it. A queued item remains inspectable
through `agentQueue/list` and becomes durable target user input when admitted. Model-authored
`send_input(..., w:"q")` and user-authored queue commands share this same target-owned FIFO;
`agentQueue/delete` removes an entry that has not begun admission. The existing `turn/started`
notification carries the queue entry and source, plus that entry's committed response policy, so
the TUI can promote pending queue presentation without a second lifecycle notification. Core
publishes the queue entry's handling only after its source-side policy commits. That same
source-side transaction consumes and binds any independently reserved next-turn policy before the
target publishes `turn/started`, combining it with the queue entry under the normal
strongest-delivery rules. A degraded post-admission start retains queue provenance with no entry
policy and emits the existing source warning; any previously durable next-turn policy remains
bound to that exact turn.

`/agent queue <target>` without a prompt opens that target's queued-follow-up view. The user can
inspect queued items there, press Enter to dequeue one and restore its complete command/input to
the composer for editing, or press Tab to open explicit edit/remove controls. Stable process-local
entry IDs keep either action bound to the selected prompt if completion concurrently drains another
item. Opening the view does not admit a turn, and no separate `pending` or `start` command exists.

## Target-turn handling

The user command should reuse the same parsed target-turn policy as model-facing agent tools:

| Flag | Effect |
| --- | --- |
| omitted | Steer active work and deliver the final response passively. |
| `c` | Deliver the first complete commentary item. |
| `f` | Deliver the final response and wake the source model if it is idle. |
| `m` | Give this target turn an attributed route back to the source, with at most one idle source wake. |
| `q` | Queue supplied input as a distinct FIFO turn instead of steering active work. |
| `x` | Keep the final response presentation-only; do not add it to source-model context. |

The displayed source thread is always the observer. Switching visual focus after dispatch does not
move the observation to another thread.

The control pane should label these modes `passive`, `wake`, and `presentation` while command entry
uses the compact shared `w` syntax. Presentation must also show whether first commentary, a reverse
message route, or queued admission was requested.

Commentary means the first complete commentary item, not streaming output. The basic pane
emphasizes passive, wake, and presentation-only final delivery; commentary remains an advanced
toggle because the user can inspect target commentary directly. It is still useful when the source
model needs the target's immediate acknowledgement or task interpretation.

`m` is directional, exact-turn authority rather than ambient graph discovery. The target model sees
the source identity and compact reply selector only after the grant binds. An accepted message
steers an active source; the first message to an idle source may start one wake turn, and later
messages may steer only that same wake turn. Completion, close, ownership transfer, process
shutdown, cold resume, and fork revoke the live route. TUI and rollout history retain the grant and
messages for audit without reactivating authority.

### Existing stronger observation

Model-facing response observation is monotonic for one observer and target turn: an earlier or
concurrent `f` wins over a later `x`. Therefore `w:x` on an active turn cannot truthfully promise
presentation-only delivery when the same source already holds a final wake for that turn.

Per-dispatch `w` preserves this invariant and explains when a stronger existing observation remains
active. A separate explicit user-authoritative observation command may replace or revoke an
existing subscription:

```text
/agent observe <target> passive
/agent observe <target> wake
/agent observe <target> presentation
```

Replacement semantics are separate from per-dispatch `w`. The explicit command is the user action
that authorizes weakening model-authored orchestration; the transcript must record the previous
and replacement modes. If a final result has already won durable delivery admission, replacement
cannot retract that committed item.

`observe` applies only to the target's active or pending turn, or to an undelivered completion
reservation for that turn. It is not a permanent agent subscription. If the target is idle with
no reserved completion, the command reports that there is no observation to replace; use `w` on
the next dispatch or resume instead. It replaces final-response handling only and does not add,
remove, or replay first-commentary observation.

Promoting a presentation-only user task to passive or wake delivery records its compact hidden
task linkage before the replacement becomes deliverable. The source model therefore receives the
question as well as the eventual answer; demotion to presentation-only does not attempt to retract
context that was already committed.

## User input and attribution

The target must receive app-server `UserInput`, including text elements, images, skills, plugin
mentions, and connected-app mentions. The command must not fabricate a model-authored
`send_input` tool call or convert structured input to a plain string.

User-authored input and model-authored inter-agent communication may share lower-level turn
admission machinery, but their durable and presentation provenance must remain distinct. The
target transcript should render this command as user input. The source transcript should render a
user-initiated agent control action with target, lifecycle action, prompt preview, and response
handling.

## Core and app-server boundary

The TUI issues the experimental typed `agent/control` app-server v2 request. Its discriminated
`action` grows with the control surface while keeping source-relative authorization and response
handling in one protocol method:

```text
agent/control {
  sourceThreadId,
  authoredSelector?,
  action: {
    type: "spawn" | "prompt" | "reservedPrompt" | "queuedPrompt"
        | "resume" | "interrupt" | "close" | "observe",
    target,
    input?, // spawn, prompt, queued prompt, or interrupt follow-up
    responseHandling?
  }
}
```

The complete request needs:

- source thread ID;
- a discriminated existing-target selector (`id`, `ref`, or `nickname`) or a
  default/configured-role spawn selector;
- the authored selector text for audit and actionable diagnostics;
- optional structured `UserInput` items, empty only for an idle child spawn;
- no-history, full-history, or last-N-turns fork mode for a spawn;
- immediate or FIFO next-turn admission;
- optional target-turn handling, including response observation, a one-shot first-turn reservation
  for an idle child spawn, and an exact-turn reverse-message grant.

The app server should locate the live source thread and invoke a shared core lifecycle operation.
The operation should reuse, rather than duplicate, the invariants behind:

- `spawn_agent_with_metadata`;
- idle child creation without a synthetic initial task;
- closed-rollout resume and live-agent adoption;
- `send_input_observing_response`;
- root-scoped ref and nickname resolution;
- the shared process-lifetime target-owned agent-turn queue;
- exact-turn observation binding;
- exact-turn reverse-message authorization and one-idle-wake accounting;
- caller-appropriate depth authority and shared concurrency checks;
- completion watcher ownership and deduplication.

Model tool handlers and the user-facing request may adapt their different inputs into the same
core operations, but the app server must not invoke a synthetic model tool call. The model did not
author the action, and audit history must not claim that it did.

The persisted alias relation is the source of truth for refs, nicknames, current root, and current
control edges. The TUI must not reconstruct aliases from graph traversal or filtered row order.
App-server v2 therefore also needs the experimental root-scoped alias projection specified by the
short-target contract:

```text
agentAlias/list { rootThreadId, cursor?, limit? }
  -> { data: [{ threadId, ref, nickname, state }], nextCursor? }
```

Spawn returns the newly committed ref and nickname with its canonical UUID. After close, transfer,
or metadata mutation, clients refresh `agentAlias/list` before rendering alias-dependent state; a
future typed alias-update notification may remove that extra read. Generic `Thread.id`, rollout
identity, and canonical lifecycle events remain UUID-based; a global `Thread.agentRef` would be
ambiguous because refs are relative to a root namespace.

Any new app-server surface should be v2, typed, schema-generated, and experimental while the
contract is fork-specific.

## Audit and presentation

The source transcript should durably show:

- whether the user sent, resumed-and-sent, spawned, or admitted a queued follow-up;
- the selector exactly as authored and its resolved canonical target UUID;
- canonical target UUID plus available numeric ref, nickname, and role;
- default or configured-role spawn selection;
- requested and effective fork mode for a new spawn;
- prompt preview;
- requested response handling;
- resulting action outcome or actionable failure.

Successful prompt and queued-prompt control items persist whether dispatch reopened the target, so
replay and pagination preserve the distinction between “sent” and “resumed and sent” without
inferring it from transient runtime status.

The target transcript should show the complete genuine user input. Completion should reuse the
existing observer-relative visible or presentation-only rows and should not duplicate the target's
final answer.

When the user authors a turn covered by a prompt-less spawn or resume reservation, the source
transcript should record an attributed task linkage without misrepresenting it as source-thread
user input.
Any model-visible commentary or final delivery must carry enough of that linkage for the source
model to interpret the response. Pure `w:x` delivery keeps both the task linkage payload and final
response out of source-model context while retaining their user-visible audit trail. With `w:cx`,
the task linkage and first commentary are model-visible while the final response remains
presentation-only.

The TUI admits that composer submission through `agent/control`'s `reservedPrompt` action. Core
consumes the durable next-turn observation installed by the prompt-less spawn or resume; it must
not register the same `w` policy again, because doing so could duplicate commentary or promote
presentation-only delivery to passive delivery.

When the response policy exposes commentary, the final response, or an `m` reply route to the
source model, Core records a compact hidden `<user_agent_task>` linkage in source model history as
part of committed target admission and before publishing response delivery. This lets a later
commentary, attributed message, or completion identify user-delegated work without turning the
slash command into source-thread user input. Pure `w:x` omits that model-context linkage as well as
the final response. Replacing one turn's policy from model-visible to presentation-only and back
does not append the same task linkage again while it remains in effective source context. The
linkage uses a trusted response-item identity and survives rollback independently of the
source-model turn, matching the durable child response that it explains. For a V1 source, this
linkage and the corresponding
`<subagent_commentary>` and `<subagent_notification>` fragments carry the canonical UUID plus any
available source-root ref and nickname. For a V2 source they carry the canonical UUID and agent
path. The model therefore sees `Main` and generated names such as `Pascal` in V1 rather than a
synthetic V2 path. The environment-context subagent roster uses those same compact V1 identities,
and token-budget context names the current V1 thread by nickname, ref, or UUID fallback instead of
collapsing every pathless V1 thread to `/root`. Native V2 inter-agent messages and token-budget
context continue using their routed agent paths.

Local TUI info messages are insufficient as the only source-side record because they disappear
from rollout audit history. The core-authored control item needs a stable identity and must project
through live, replay, non-paginated, and paginated transcript paths.

Agent-context forks do not copy these source-relative control items into the child transcript.
The canonical source rollout retains the complete audit, while the child receives only the
sanitized model context selected by its fork mode.

If target mutation commits but source audit persistence fails, app-server returns the typed
operation outcome with an `auditWarning`. The TUI applies that outcome, removes an already-admitted
queued item, and clearly reports the missing audit instead of presenting a retryable failure that
could duplicate the target turn.

Target input admission is also the retry-safety boundary when exact-turn response handling cannot
be persisted afterward. Core rolls back or quarantines the uncertain response observation, returns
the admitted submission with a `postAdmissionWarning`, and does not publish the failed binding.
The source audit remains successful but renders the warning, and the TUI must not retry or retain a
queued prompt whose target turn already started.

Canonical agent lifecycle events stay UUID-authoritative and do not gain a required ref. The alias
store and the source control item preserve alias auditability. An adoption item additionally
records old root, new root, target UUID, and the authored selector; historical rollout parent and
source fields continue to describe history rather than being rewritten to the new owner.

Before admission, a queued follow-up remains process-local structured input in the shared
target-owned FIFO. It is not yet target model input. Its durable source control item records queue
acceptance; when the entry drains, the target receives the complete user input and the queued
response policy binds to that exact turn.

## Approvals

Approvals are not a separate `/agent` action. Delegated subagent approval requests already route
to the parent TUI's normal approval overlay. Requests associated with an inactive thread already
surface through the pending-thread indicator and appear through the normal UI when that thread is
selected.

The control pane may mirror this pending status and provide navigation to the owning thread, but
it must not defer, collect, or independently resolve approval requests. There is no separate
`/agent approve` command or agent-control approval queue.

## Reviews

Review work has no dedicated `/agent` verb or special lifecycle state. A user can spawn a
configured reviewer role, prompt an existing agent to review, or ask an agent to spawn its own
reviewer. Reviewer threads remain ordinary agents. The built-in `/review` workflow remains
independent from the agent control pane.

## Lifecycle and race invariants

- Resolve the source, target, and source presentation before mutation.
- Allocate a child alias and current parent edge in one durable transaction before publishing the
  child to the registry, TUI, app-server clients, or model tool result.
- For a history-bearing root fork, import inherited ref and nickname reservations before publishing
  the fork through the loaded-thread map or thread-created notification. A concurrent first child
  must never claim an inherited selector.
- Serialize concurrent allocations with the alias namespace high-water mark and uniqueness
  constraints. Reject nickname collisions before publication so a later spawn can resynchronize
  durable reservations and choose another candidate.
- Load or transactionally backfill the root alias namespace before accepting short selectors after
  cold resume.
- Reject self-resume, self-close, and self-observe semantics. A prompt, queued follow-up, or
  interrupt for the currently displayed thread should use the normal composer or shortcut.
- Reject child-to-Main close while preserving an exact-turn `m` route and independent response
  observation.
- Resolve refs and nicknames only in the source root. Reject out-of-root UUID prompt, queue, wait,
  observe, interrupt, and close; only an explicit resume/adopt operation may transfer ownership.
- Make ownership transfer exclusive and transactional across the complete persisted subtree.
  Competing adoptions have one winner, old subscriptions are revoked, and neither the target nor
  a descendant can remain current in two roots.
- Acquire exclusive live-writer ownership before committing an out-of-root transfer. Process-local
  loaded-thread checks provide an early actionable error, while the writer lease remains the
  cross-process exclusion boundary. Do not use durable alias state as a liveness heartbeat.
- Route user, model, app-server, and generic resume entry points through the same ownership-aware
  Core operation. Transport-specific audit and response-observation behavior may differ, but
  ownership, parent preservation, transfer, liveness, and publication ordering may not.
- Bind observation to the exact target turn admitted by this dispatch.
- Preserve the target's persisted lifecycle parent on same-root resume even when another sibling
  initiates the operation; response observation remains bound to the initiating source.
- Bind a queued follow-up's observation only when its own future turn is admitted.
- Cancel process-local queued follow-ups when either their target or authoring source thread closes.
- Use one target-owned FIFO for user and model queued prompts so client origin cannot change
  ordering, cancellation, or idle-turn reservation.
- Preserve one final delivery when resume, start, completion, wait, close, or another dispatch
  races.
- If an active turn completes while a follow-up is being queued, admit that follow-up exactly once
  as the next turn.
- Do not create duplicate completion watchers when adopting an already live target.
- Do not resurrect a target while a close is still committing; return an actionable retry result.
- Preserve successful final responses and terminal errors through passive, wake, and
  presentation-only delivery.
- Do not restore live ownership or response subscriptions across process restart, cold resume, or
  fork without a new explicit user command.
- Do not copy alias mappings into a root fork. A history-bearing fork reserves inherited numeric
  and nickname space, while a no-history fork starts a fresh namespace.
- Apply the current source thread's concurrency limit to resume and spawn. Apply its depth limit to
  model-authored operations that create a new parent edge. Explicit user spawning and adoption may
  exceed that autonomous depth budget while recording the actual depth; live observation and
  same-root resume retain the target's existing graph depth.
- Keep V1 `send_input` available at maximum spawn depth so leaf agents can notify Main or
  same-root siblings through an explicitly granted route without opening a new lifecycle
  relationship.
- Do not treat a known UUID as reverse-message authority. Bind `m` to the exact target turn, allow
  at most one idle source wake, and revoke it on terminal lifecycle, ownership, or cold-process
  boundaries.
- Persist and render every accepted reverse message with agent attribution; never present it as
  genuine user input.
- Keep the displayed source thread usable if target resume, spawn, or prompt admission fails.

## V1 and V2

The user-facing grammar should not expose the selected multi-agent implementation version.
The full surface is complete only when both control-plane adapters implement the required
semantics:

| Capability | V1 adapter | V2 adapter |
| --- | --- | --- |
| same-root prompt, queue, interrupt, close | Use UUID-resolved `AgentControl` lifecycle operations. | Resolve the durable alias to a UUID, then use the V2-loaded target's shared user-input and lifecycle operations. |
| source-relative `c`/`f`/`x` handling | Bind the durable exact-turn response observer. | Bind the same durable exact-turn observer so `w` semantics do not change or duplicate native V2 completion delivery. |
| `m` reverse route | Bind a one-target-turn V1 send capability to the source identity and one idle wake. | Adapt native V2 agent-message routing to the same source-relative capability and wake limit. |
| `q` queued input | Use the shared target-owned FIFO and bind policy at exact future-turn admission. | Use the same FIFO ahead of V2 task admission rather than the native mailbox as a second queue. |
| default or role spawn | Use the V1 child registry and shared role configuration. | Use the V2 task graph and shared role configuration. |
| `fork:none`, `fork:all`, `fork:N` | Adapt the shared user-dispatch fork mode without changing the model-tool schema. | Adapt the same fork mode to V2 task spawning. |
| current-root closed resume | Reopen through the existing V1 control edge. | Reopen through the existing V2 task/control edge. |
| out-of-root UUID adoption | Perform the shared exclusive transfer through explicit `resume`. | Perform the same shared exclusive transfer through explicit `resume`. |
| durable aliases and source audit | Use the shared root alias store and control item. | Use the same store and item; task paths remain internal metadata. |

Version-specific task names, mailboxes, and watcher implementation remain internal. A
user-created V2 child receives an opaque canonical task path for native V2 identity, while the
pane and commands continue to address it by UUID, numeric ref, or nickname. V1 model-facing
follow-up tools also accept refs and nicknames; V2 model tools may retain their existing task-path
contract. Both versions project user-facing refs from the same root-scoped alias store, and neither
version may infer them from task paths or picker order. An operation whose selected adapter is
incomplete must remain disabled or return a version-neutral actionable error before mutation; it
must not silently weaken input provenance, observation, ownership, fork, or audit semantics.

## Command semantics

- bare `/agent` shows the native graph and status summaries.
- `id:`, `ref:`, and `nick:` force an existing-target namespace; `role:` forces a configured-role
  spawn. Unprefixed action words, UUIDs, decimal refs, roles, and nicknames follow the documented
  precedence.
- `new` spawns a default child with the requested fork mode; without a prompt it reserves the
  optional one-shot first-turn observation policy and switches into the blank child TUI, while a
  prompt starts the child without switching.
- `<target>` without a prompt opens that agent's Overview/Inspect view; with a prompt it dispatches
  immediately.
- `main` in any case variation is the reserved nickname for the current root, so
  `/agent main [w:<w-mode>] <prompt>` works from a child without a preceding resume.
- `<role>` spawns a configured child with the requested fork mode. Without a prompt it reserves its
  optional one-shot first-turn observation policy and switches into its blank TUI; adding a prompt
  starts the new child's first turn without switching.
- `queue <target>` opens that target's queued-follow-up view; adding a prompt defers it until the
  active turn completes, or dispatches immediately when the target is already idle or closed.
  `w:q` on ordinary prompt syntax selects the same path.
- `interrupt` stops the active turn but leaves the agent live. An optional follow-up starts the
  next turn after interruption commits, with `w` bound to that follow-up turn.
- `close` ends the agent runtime, revokes pending observation, and conditionally replays a
  completed response according to `w`.
- `resume` reopens or adopts without sending a prompt; optional response handling binds to the
  next admitted turn under the existing next-turn policy.
- `observe` explicitly replaces source-relative response handling when replacement remains
  possible.
- `w:m` grants only the resulting target turn a reverse-message route to the displayed source;
  `w:q` carries every other selected flag with the future queued turn and queues its model-visible
  final response for the source's next turn.

Prompt text must not collide with action verbs. Except for the documented target-first `close`
form, verb forms are parsed only immediately after `/agent`; `/agent <target> stop ...` remains
ordinary prompt text unless an explicit compatibility alias is deliberately added.

## Required coverage

Integration and TUI coverage should include:

1. Active target receives structured user input through steer without changing source focus.
2. Idle target starts with sticky settings and omitted `w` delivers passively.
3. Closed known descendant resumes, receives the prompt, and retains metadata.
4. Manually entered out-of-root historical UUID rejects prompt dispatch, then explicit `resume`
   adopts it even when absent from current navigation and permits a later prompt.
5. Missing, archived, closing, and malformed UUIDs return actionable errors without mutation.
6. Default and exact-role dispatches spawn new children with no context by default, including
   repeated dispatches for a role already in use.
7. An exact role name wins over a colliding ordinary nickname, while the existing agent remains
   addressable by ref, UUID, or `nick:`; the reserved `Main` target wins over an unprefixed role
   named `main`, which remains available through `role:main`.
8. `w:f` wakes the displayed source exactly once.
9. `w:x` produces presentation-only completion on a new turn.
10. `w:m` exposes an attributed reply route for one target turn, permits one idle source wake, and
    rejects later wake attempts or UUID-only bypass.
11. Existing `f` remains authoritative when a later active-turn dispatch requests `x`.
12. Source switch after dispatch does not move observation ownership.
13. Child-to-sibling dispatch binds the child as observer.
14. Concurrent close, resume, prompt, completion, and wait races preserve one target turn and one
    final delivery.
15. Source and target transcript items survive replay, compaction, rollback, and pagination.
16. V1 and V2 expose equivalent user-visible behavior.
17. Queueing an active-target follow-up does not steer the current turn and starts exactly one
    subsequent turn.
18. A turn-completion race either queues or immediately admits the follow-up without loss or
    duplication.
19. An idle or closed target given `queue` dispatches immediately, while a configured role is
    rejected as a non-target.
20. A queued follow-up's `w` policy binds to its future turn rather than the turn active at queue
    time.
21. User `/agent queue` and model `w:q` preserve one shared FIFO across mixed-origin entries.
22. A queued target turn's final response waits for the source's next-turn boundary instead of
    steering active source work.
23. Close suppresses an exact response still in effective model history, restores it after a
    replacement boundary removes it, and honors passive, wake, queued, and presentation-only
    handling.
24. Interrupt with a follow-up orders cancellation before next-turn prompt admission and binds
    its optional `w` policy to that new turn.
25. Control-pane Overview derives summaries from canonical state without copying transcript
    history.
26. Inspect, Back, and full thread switch preserve source pane and transcript navigation state.
27. Subagent approvals use the normal parent-TUI approval flow, and the control pane only mirrors
    the owning thread's pending status.
28. Reviewer roles and review prompts behave as ordinary agent work without a special review
    lifecycle.
29. Prompt-less default or role selection allocates a real idle child with the applicable
    settings, parent edge, canonical rollout identity, and no inherited history, then switches
    into it.
30. Omitted, `f`, and `x` observation on a prompt-less default or role spawn bind once to its first
    user turn and do not observe later turns.
31. Closing an idle child before its first turn revokes the reservation without waking its parent.
32. Queued follow-ups can be inspected, edited, and removed without switching the primary
    transcript.
33. Prompt-less resume establishes or adopts live control without starting a turn; its optional
    `w` policy binds once to the next admitted target turn. Prompt-bearing resume admits that turn
    under the same reserved policy.
34. Repeated resume is idempotent and does not create duplicate completion watchers.
35. `observe` replaces only active, pending, or reserved exact-turn observation, expires with that
    turn, and reports no applicable observation for an unrelated idle target.
36. Self-resume, self-close, and self-observe are rejected without affecting the displayed
    thread.
37. Reserved action-, option-, numeric-, UUID-, whitespace-, and role-shaped collisions remain
    reachable through forced `id:`, `ref:`, `nick:`, and `role:` selectors and autocomplete emits
    an unambiguous form.
38. Wide split-pane and narrow stacked layouts have snapshot coverage with text-visible statuses,
    disabled reasons, response mode, pending approval status, and nested agents.
39. Omitted and explicit `fork:none` produce no inherited parent conversation in V1 and V2.
40. `fork:all` preserves the parent's effective rollback- and compaction-aware model context in
    non-paginated and paginated histories.
41. Positive `fork:N` retains exactly the shared last-N fork-turn slice in V1 and V2, while zero,
    negative, boolean, duplicate, and malformed values fail before spawning.
42. A prompt-less fork snapshots parent context at child creation rather than first-turn
    submission.
43. Adding numeric user-dispatch forks does not change the generated model-visible V1
    `spawn_agent` schema.
44. Every existing-target user action resolves the same UUID from `id:`, `ref:`, `nick:`, its
    unprefixed full ID, stored numeric ref, or exact unambiguous ordinary nickname; `Main` resolves
    the root case-insensitively.
45. `/agent` displays Main as ref `1` with reserved nickname `Main`, and stable monotonic
    descendant refs that do not change under filtering, close, role-row insertion, or metadata
    refresh.
46. Cold resume loads committed refs or transactionally backfills open and closed descendants in
    deterministic graph order without consuming live capacity, exposing partial state, or reusing
    aliases; concurrent processes converge on the same map.
47. A no-history root fork starts a fresh alias namespace. A history-bearing root fork imports
    numeric and nickname reservations but no old mappings or subscriptions, so inherited aliases
    remain unknown; explicit UUID adoption receives fresh fork-scoped aliases.
48. V1 model follow-up tools accept the same refs and nicknames while canonical events and
    app-server identities retain the resolved UUID.
49. Concurrent sibling and multi-process spawns allocate unique monotonic refs and never publish
    an alias before the alias-and-edge transaction commits.
50. Crashes before and after alias commit leave either no published alias or a recoverable
    committed alias, never a model-visible uncommitted ref.
51. Same-root prompt, queue, wait/observe, interrupt, and close succeed subject to lifecycle
    checks, while equivalent out-of-root UUID operations reject without mutation.
52. A child cannot close Main, while an exact-turn `m` grant permits attributed child-to-Main
    input and independent response observation.
53. Two roots attempting to adopt one target produce one exclusive owner, one auditable transfer,
    and one actionable conflict without duplicate watchers.
54. Adoption preserves historical rollout source and parent metadata while its control item
    records old root, new root, UUID, and authored selector.
55. Root-scoped app-server alias list, mutation responses or update notifications, replay, and
    pagination expose the same committed aliases without synchronous render-time metadata reads.
56. `/agent fork:none`, `fork:all`, and `fork:N` children remain in the source alias namespace;
    only a separate root-thread fork creates a new namespace.
57. Canonical lifecycle records remain UUID-authoritative while the user-control audit item
    preserves authored selector and resolved UUID.
58. A V1 child at maximum spawn depth still receives `send_input` and can use an explicit
    same-root `m` route without receiving deeper spawn/resume/wait/close tools.
59. Adopting a stored foreign root preserves its UUID and history while assigning a generated
    destination child nickname instead of importing `Main`.
60. Parent, child, and sibling resume entry points preserve an existing same-root parent/depth and
    generic resume cannot reopen a transferred V1 rollout under its historical owner.
61. Explicit user spawn/adoption succeeds beyond the autonomous model depth budget, records its
    real depth, and leaves the resulting leaf with communication-only V1 tools.
62. A former owner cannot use stale same-root authority after transfer, but may explicitly adopt
    the closed rollout back with its latest history and reserved destination identity.
63. A live writer in another app-server rejects adoption before durable aliases transfer; after
    that writer closes, the same resume succeeds without process discovery.
64. A history-bearing root fork cannot be discovered or spawn a first child until inherited ref
    and nickname reservations commit.

Tests should use deterministic lifecycle and response gates rather than sleeps.

## Suggested implementation sequence

1. Add the durable root alias store, allocation/transfer transactions, deterministic pre-alias
   backfill, shared selector resolver, and experimental root-scoped app-server alias projection.
   Do not publish model- or user-facing refs before this foundation commits them.
2. Extract shared core user-agent dispatch operations from existing lifecycle handlers without
   changing model-tool behavior.
3. Add typed app-server v2 dispatch requests and source-relative audit items for prompt, resume,
   spawn, and transfer.
4. Implement active- and idle-agent direct prompts and native queued follow-ups with typed target
   autocomplete.
5. Expand bare `/agent` into a read-only Overview/Inspect pane backed only by canonical graph,
   alias, transcript, and approval state.
6. Add atomic known-descendant resume-and-prompt, explicit out-of-root adoption, and observation
   binding.
7. Add reusable immediate and idle default or role spawning, unified none/all/last-N fork modes,
   one-shot first-turn `w` reservations, autocomplete, and stronger-existing-observation guidance.
8. Add interrupt, close, standalone resume, and user-authoritative observation replacement.
9. Project pending approval status and transcript inspection into the control pane without
   duplicating their subsystems.
10. Replace process-local client-specific prompt queues with one target-owned FIFO, then add
    model-facing `q` admission and exact-turn `m` reverse-message grants.
11. Validate the complete contract in V1, preserve it through V2, and cover live, replay,
    non-paginated, and paginated presentation.

## Open decisions

- Should archived historical rollouts require an explicit unarchive action before adoption?
- Should configured roles eventually select environment or worktree templates as well as model and
  instructions?
- Which control-pane actions need direct keys versus a contextual action menu at narrow widths?
