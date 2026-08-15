# Multi-agent V1 short targets

This document defines durable alias storage and model-tool target resolution. The user-facing
`/agent` command surface is defined in [TUI agent control](tui-agent-control.md).

## Summary

Multi-agent V1 accepts three target forms: a root-scoped numeric agent ref, a
nickname, or the canonical full UUID. The numeric ref is the preferred
model-authored form and is the same stable number shown beside the agent in
`/agent`. Ordinary nicknames are exact readable aliases; the root also owns the
reserved case-insensitive nickname `Main`. UUIDs remain the durable external
identity and recovery fallback.

`multi_agent_v1.spawn_agent` returns all three identities when the child has a
durable alias:

```json
{
  "agent_id": "019faa07-aa3d-78d3-9eca-66cd8626adad",
  "nickname": "Parfit",
  "ref": "2"
}
```

Routine follow-up calls use the compact ref:

```json
{"target":"2","message":"Continue"}
```

Refs are persisted spawn ordinals, not filtered-row indices or truncated UUIDs.
Filtering, closing, or reopening agents does not renumber them.

## Goals

- Reduce the model-visible token cost of routine multi-agent tool calls.
- Make targets easy for the model and humans to associate with an agent.
- Preserve stable targeting across cold resume.
- Make the `/agent` number and model-visible short target one identity.
- Keep full UUID targeting available for compatibility and recovery.
- Preserve UUID as canonical thread identity in core, rollouts, app-server
  requests, notifications, logs, and TUI navigation.
- Produce explicit errors when a target is missing or no longer controlled by
  the current root.

## Non-goals

- Replacing thread UUIDs as canonical identifiers.
- Changing app-server's client-facing thread identity.
- Deriving identity from a partial UUID.
- Adding ephemeral aliases that are valid only for one process lifetime.
- Reusing a numeric ref within one root lineage.
- Making V1 adopt the complete V2 agent-path model.

## Target syntax

The existing V1 `target`, `targets`, and `id` fields accept:

1. A persisted decimal agent ref, preferred for model-authored calls.
2. A persisted exact agent nickname, including the reserved `Main` root nickname.
3. A full thread UUID, retained as the canonical fallback.

Examples:

```json
{"target":"2","message":"Continue"}
```

```json
{"targets":["2","3"],"timeout_ms":30000}
```

```json
{"target":"Parfit","message":"Continue"}
```

```json
{"target":"main","message":"Status update from a child","w":"cx"}
```

The child-to-Main example assumes that child's current turn holds the exact-turn `m` route granted
by Main. The selector identifies the destination; it does not itself authorize reverse input.

```json
{"target":"019faa07-aa3d-78d3-9eca-66cd8626adad"}
```

Explicit selectors are also accepted when a human-authored or restored alias
collides with another lexical form:

```json
{"target":"ref:2","message":"Continue"}
```

```json
{"target":"nick:Ada Lovelace","message":"Continue"}
```

```json
{"target":"id:019faa07-aa3d-78d3-9eca-66cd8626adad"}
```

This applies consistently to:

- `send_input.target`
- `wait_agent.targets`
- `resume_agent.id`
- `close_agent.target`

Tool descriptions tell the model to use the short ref returned by
`spawn_agent`, use the nickname when it improves readability, and use the full
UUID only when needed. The existing field names remain unchanged; in
particular, alias support broadens the accepted values of `resume_agent.id`
rather than renaming it to `target`.

## Resolution rules

Target resolution is deterministic:

1. Resolve `id:`, `ref:`, and `nick:` prefixes in their forced namespace.
2. Otherwise, parse a canonical full `ThreadId`.
3. Otherwise, parse a canonical unsigned decimal ref.
4. Otherwise, perform a nickname lookup.
5. Return the resolved thread UUID to the existing lifecycle implementation.
6. If there is no match, return an error that identifies the unknown target.

Ordinary nickname matching is exact and case-sensitive. `Main` is the sole
case-insensitive exception, so `main`, `Main`, `MAIN`, and `nick:mAiN` resolve
the current root. The alias store enforces one resolvable nickname per root
namespace, including reservations retained after close or ownership transfer.
Fuzzy matching remains unsupported.

The resolver is shared by all V1 follow-up tools so their accepted
syntax and error behavior cannot drift.

The namespaces are not intrinsically disjoint. Configured nickname candidates
can contain spaces, consist only of digits, resemble a UUID, or equal a
user-command action. Unprefixed UUID and decimal syntax therefore win over
nickname lookup; `nick:` is the escape hatch. Newly allocated aliases should
avoid these forms when possible, but restoration must not silently rename
historical metadata merely to simplify parsing.

## Durable alias store

Refs and resolvable nicknames come from a state-store relation, not the
in-memory registry, the generic picker row number, or inferred rollout order.
Use the existing root-shared `SessionId` as the alias namespace key.

The store maintains:

- one namespace row containing the root session ID and next numeric ref;
- one alias row keyed by `(root session ID, thread UUID)`;
- unique `(root session ID, numeric ref)` and resolvable-nickname constraints;
- an alias state of active, closed, or transferred;
- durable nickname reservations so a closed or transferred alias is not reused.

The database may store refs as integers, but model and TUI APIs serialize them
as decimal strings so they can be copied into existing string target fields.
Main receives ref `1` and the canonical nickname `Main` when the namespace is
created. New descendants receive the next monotonic ref. Numeric refs and
nickname aliases are never reused within that namespace, and child nickname
allocation excludes every case variant of `Main`.

Nickname allocation continues to use the default or role-configured candidate
list and its existing ordinal-suffix generations. Every retained alias is a
durable reservation. Allocation scans successive generations until it finds an
unreserved candidate, so a separate persisted generation counter is
unnecessary. A database uniqueness conflict from another process fails before
publishing the child and asks the caller to retry. The next spawn synchronizes
the winning reservation and chooses another candidate rather than clearing
reservations or exposing an uncommitted alias.

### Allocation transaction

Thread creation, alias allocation, and publication have an explicit order:

1. Create and durably identify the child rollout.
2. In one state-store transaction, lock the namespace, increment its
   high-water mark, insert the alias, and persist the current parent edge.
3. Commit that transaction.
4. Only then publish the child in the live registry, render it in `/agent`, or
   return the model-visible `spawn_agent` result.

Concurrent sibling spawns and multiple processes must use database
serialization and uniqueness rather than racing in-memory counters. A
nickname collision fails before publication with an actionable retry; the
next spawn synchronizes the winning reservation before choosing a new
candidate. A crash before step 2 can leave an undiscovered rollout addressable
later by UUID. A crash after step 3 is recoverable from the committed alias
row. It must never return a ref that was not durably committed.

## Cold resume

An in-memory spawn counter or live-agent-only alias table would be cleared on
cold resume while old tool calls remain in model history. Reassigning `"2"` to
a different thread would be worse than rejecting it.

Cold resume loads the alias namespace before accepting short-target calls:

- every thread controlled by the root resolves the same map;
- a child can address Main as `"Main"` (case-insensitively) or `"1"`, and a
  sibling by its displayed ref, when the requested operation is otherwise
  authorized;
- closed descendants retain aliases without consuming live-agent capacity;
- nickname allocation resumes after all reserved aliases, not merely currently
  live nicknames.

For a pre-alias root with no alias namespace, perform one deterministic
transactional backfill over descendants known to the persisted graph. When an
older V1 rollout retains parent ancestry that is missing from that graph,
recover and persist only those rollout-backed edges before allocating aliases.
Current storage does not retain historical spawn order, so the backfill order is
the store's stable graph order: breadth/depth first and canonical UUID within
each level. This is a deterministic surrogate, not a claim about original spawn
order. Main is `1`/`Main`; descendants start at `2`.

Graph- or rollout-known UUIDs can receive refs even when optional nickname
metadata is unreadable; omit the nickname and warn with the UUID. A thread with
neither persisted graph ancestry nor trustworthy rollout ancestry remains
UUID-only until explicit adoption. In a partially migrated namespace, allocate
missing entries above the persisted high-water mark rather than filling holes.
A failed transaction exposes no partial ref map and retains UUID fallback.

## Forks

This section describes a root-thread fork such as `codex fork`, not a child
spawn that receives some parent model context. A context-forked child remains a
descendant in the source root's alias namespace.

A root-thread fork creates a new alias namespace and never copies live
ownership or response subscriptions.

- `fork:none` inherits no alias-bearing model history, so the new root starts
  with Main `1`/`Main` and the next ref `2`.
- `fork:all` and `fork:N` rebind only Main as `1`/`Main`. They copy neither old
  child refs nor nickname mappings. Old refs other than `1` and old child
  nicknames therefore resolve as unknown in the fork.
- A history-bearing fork seeds its numeric high-water mark above the source
  namespace and imports the source namespace's nicknames as reservations only.
  This prevents inherited calls from silently targeting unrelated new agents.
- Explicit UUID adoption assigns a fresh fork-scoped ref and a non-conflicting
  live nickname alias. When the UUID identifies another root's Main, the
  adoption boundary uses the normal child nickname generator rather than
  importing the source root's reserved `Main` identity.

The source UUIDs and historical nickname metadata remain visible in inherited
audit history. Alias reservations prevent misresolution; they do not pretend
those old agents are still controlled by the fork.

## Ownership transfer and authorization

Alias resolution and lifecycle authority are separate decisions. Refs and
nicknames resolve only inside the caller's root namespace. Supplying a stored
rollout's full UUID to explicit `resume_agent` authorizes loading that rollout
with its existing history and adopting it as a child. The UUID may identify
another root's Main, a descendant in another root, or an unaliased standalone
rollout. Other operations do not implicitly reparent an out-of-root rollout,
and a runtime still live under another root cannot acquire a second controller.

| Operation | Same-root ref, nickname, or UUID | UUID outside current root |
| --- | --- | --- |
| send, wait, interrupt, observe, close | Allowed when existing lifecycle and scoped-message rules permit it. | Reject as not controlled. |
| resume known closed descendant | Resume under the existing root. | Not applicable. |
| explicit resume/adopt | Idempotent when already controlled. | May transfer exclusive ownership after validation. |
| inspect | Allowed. | Read-only lookup may be allowed without mutation. |

Main cannot be closed by a child. Self-send, self-wait, self-resume, and
self-close are invalid. Child-to-Main and peer communication use an exact-turn
`m` route granted by the target that dispatched the work; knowing a UUID or
alias does not create that authority. At V1's maximum spawn depth, `send_input`
remains model-visible so a leaf can use a granted route without receiving tools
that create or manage deeper lifecycle relationships. The depth budget limits
autonomous model-created edges, not explicit user `/agent` spawn or adoption;
those operations record their actual depth and retain normal concurrency
limits.
Other same-root parent, child, and sibling actions keep their existing
operation-specific checks.

Adoption is exclusive ownership transfer, not a second simultaneous controller
edge. An out-of-root target—including another root's Main—or one of its
persisted descendants must not still be live under its existing controller;
otherwise resume rejects with guidance to close the subtree first. Once the
complete subtree is stored and not live, adoption first acquires the rollout's
exclusive live-writer lease and then performs one transaction that:

1. validates that the target is resumable and not closing;
2. snapshots the complete persisted subtree, rejects mixed ownership, and tombstones its
   old-root aliases while removing old live subscriptions;
3. allocates new-root refs and available nickname aliases for the target and every descendant;
4. replaces the target's persisted parent/control edge while keeping descendant edges attached;
5. records an auditable transfer containing old root, new root, target UUID,
   and authored selector.

The selected target reuses any alias already reserved for it in the destination
root. Otherwise its newly selected nickname must still be available when the
transaction commits; a concurrent collision rejects the adoption before old
ownership is marked transferred so a retry can select another identity.

An unavailable descendant nickname is omitted in the destination namespace rather than aborting
the ownership transfer. Its canonical UUID and new numeric ref remain available, while the old
namespace retains the historical nickname reservation.

The runtime may be prepared behind the exclusive writer lease, but only after
the transfer commits may the new root publish the target or admit input.
Historical rollout `SessionSource`, original parent metadata, and canonical
UUID remain unchanged. The persisted graph represents current exclusive
control; the transfer audit record preserves how ownership changed.

The current durable alias identifies an owner, not a running process. Within
one app-server, the shared loaded-thread map rejects a target or descendant
already live under another root. Across app-server processes sharing one thread
store, exclusive live-writer acquisition is authoritative and a conflict leaves
the old aliases unchanged. Resume does not scan processes or depend on an
`active` alias as a heartbeat.

User, model, app-server, and generic thread-resume callers share this ownership
decision. A source-relative same-root resume preserves the existing parent and
depth. A source-relative foreign resume transfers ownership beneath its caller.
A standalone resume with no caller reopens the current durable owner, or the
persisted root identity for an unowned rollout.

The UUID remains available for same-root recovery when old rollouts lack alias
metadata or reconstruction fails. An out-of-root UUID remains inspectable where
permitted, but mutation still requires the explicit adoption path above.

## Why not truncated UUIDs

A raw UUID suffix such as `"d"` is not a safe identifier:

- One hexadecimal character has only 16 possible values, fewer than a
  configuration that permits 20 concurrent agents.
- Two- and three-character suffixes can still collide.
- A suffix that is unique when returned can become ambiguous after a later
  spawn.
- Variable-length "shortest unique suffix" references are therefore not stable
  over the lifetime of a thread.
- Suffixes are opaque to the model and humans compared with a nickname.

If partial UUIDs were accepted, ambiguity would need to be handled explicitly,
but their instability provides little benefit over persisted refs and
nicknames.

## Durable numeric refs and `/agent`

The existing number rendered by the generic selection list is not the ref. It
is recomputed from filtered enabled rows and can change during one popup
session. The agent control pane must render the persisted ref explicitly:

```text
1  Main [default]
2  Parfit [reviewer]
3  Socrates [explorer]
```

Filtering, closing, disabling, or inserting configured-role rows must not
renumber real agents. Role and “new default agent” rows have no ref until a
spawn commits. The same displayed value is accepted by:

- `/agent <target>` and all user-facing agent actions;
- V1 `send_input`, `wait_agent`, `resume_agent`, and `close_agent`;
- any future target autocomplete or model guidance.

Refs are aliases only. Core ownership, app-server requests, rollouts,
notifications, logs, and canonical events continue using the resolved thread
UUID. Canonical collab lifecycle items remain UUID-authoritative; they do not
need a required ref field. Model function-call arguments already preserve an
authored short selector. A user-control audit item stores both its authored
selector and resolved UUID. Model-context task, commentary, and completion
fragments retain that UUID while also carrying the current root-scoped ref and
nickname, so the model can associate the result with `Main`, `Parfit`, or the
same compact target accepted by its V1 tools. The model-visible environment
roster uses the same ref and nickname instead of falling back to a V1 thread
UUID. Context-window metadata identifies the current V1 thread by nickname,
then ref, then UUID fallback; this prevents full-history V1 children from
silently retaining the parent's `/root` token-budget identity.

## App-server projection

The TUI cannot display the same persisted ref returned to the model by deriving
it from `thread/list` order or performing one metadata read per row. The
experimental app-server v2 API therefore exposes a root-alias listing keyed by
the displayed source/root thread:

```text
agentAlias/list { rootThreadId, cursor?, limit? }
  -> { data: [{ threadId, ref, nickname, state }], nextCursor? }
```

The response resolves the source thread to its root `SessionId` and returns
aliases from that namespace. Agent-control mutation responses carry the
committed alias, and clients refresh the canonical root listing after lifecycle
or metadata mutations. Rendering does not perform synchronous metadata reads.

Keep `Thread.id`, canonical item receiver IDs, rollout IDs, and generic
`thread/list` identities as UUIDs. A bare `agentRef` on a global `Thread`
projection would be ambiguous because refs are namespace-relative; the
root-keyed alias projection owns that relationship.

## Spawn result

The spawn result retains the existing identities and adds one compact field:

```json
{
  "agent_id": "019faa07-aa3d-78d3-9eca-66cd8626adad",
  "nickname": "Parfit",
  "ref": "2"
}
```

Tool guidance prefers `ref` for routine follow-ups. Returning it once
saves tokens across every later target call. The TUI spawn presentation shows
the same ref. It is serialized as a string so the returned value can be copied
directly into the existing string-valued `target`, `targets`, and `id` fields.

Omitting `agent_id` from model-visible output would save more tokens, but it is
a separate compatibility decision. Existing prompts, tests, and consumers may
expect the field, and the UUID remains useful for diagnosing missing metadata.
Any later omission must preserve the UUID in canonical events and external
APIs.

## Compatibility

- Existing same-root UUID calls continue to work unchanged.
- Arbitrary out-of-root UUID send, wait, interrupt, observe, and close are rejected;
  only explicit resume/adopt may transfer control. This is an intentional
  authorization hardening, not an alias-parser side effect.
- Existing rollouts remain replayable.
- App-server thread IDs remain UUIDs; the experimental root-alias projection is
  required for TUI display and autocomplete.
- `spawn_agent` adds `ref` without removing `agent_id` or `nickname`.
- `target` and `targets` are already semantically neutral names, so accepting a
  ref or nickname broadens their input domain without adding parallel
  parameters.
- `resume_agent` keeps its existing `id` field and broadens that field to accept
  a ref, nickname, or full UUID.
- Canonical collab tool-call items should continue recording resolved thread
  UUIDs and ordinary agent metadata. The alias store and authored function-call
  argument provide ref auditability without requiring refs on every canonical
  lifecycle item.
- V2 model tools keep their existing task-name/path contract; numeric refs are
  required for the version-neutral user `/agent` surface and V1 model tools.

## Error behavior

Representative errors:

```text
agent ref "7" was not found in this root
```

```text
agent target "Parfit" was not found
```

```text
agent "019f..." is not controlled by this root; use explicit resume/adopt
```

Malformed UUID-like strings should follow the same target-resolution rules
rather than receiving a special partial-UUID interpretation.

## Coverage

Coverage must include:

- Main receiving ref `1` and reserved case-insensitive nickname `Main`, with
  descendants receiving monotonic root-scoped refs.
- Every V1 follow-up tool resolving an exact numeric ref.
- Each V1 follow-up tool resolving an exact ordinary nickname and all case
  variants of `Main`.
- Full UUID behavior remaining unchanged.
- `resume_agent.id` accepting refs and nicknames without adding or requiring a
  `resume_agent.target` field.
- `spawn_agent` returning the same ref shown by `/agent`.
- Filtering and configured-role rows not renumbering displayed refs.
- Unknown numeric refs returning a model-visible error without UUID suffix
  matching.
- Unknown nicknames returning a model-visible error.
- Ref and nickname resolution after cold root resume.
- Ref and nickname resolution for a closed descendant after cold root resume.
- Closed or unloaded aliases not counting against concurrent live-agent
  capacity.
- A later spawn not reusing a closed descendant's ref or reserved nickname.
- A no-history fork starting a fresh alias namespace, while a history-bearing
  fork preserves numeric and nickname reservations without copying mappings or
  subscriptions.
- Old fork refs other than rebound Main `1` and old nicknames resolving as
  unknown rather than targeting new agents.
- Explicit UUID adoption assigning a new namespace-scoped ref and transferring
  exclusive control without rewriting historical rollout metadata.
- Partial reconstruction failure preserving UUID fallback without reusing
  unresolved aliases.
- Deterministic graph-order backfill for old roots with no refs, partial
  migration assigning above the high-water mark, and failed backfill exposing
  no partial map.
- Concurrent sibling/process allocations producing unique monotonic refs and
  respecting durable nickname reservations across suffix generations.
- Crash boundaries before and after alias/edge commit never exposing an
  uncommitted ref.
- Numeric-, UUID-, action-, option-, whitespace-, and role-shaped selector
  collisions resolving through explicit selector syntax.
- Full-history and last-N forks preventing both ref and nickname reuse.
- Out-of-root UUID rejection for send, wait, interrupt, observe, and close.
- Self-target rejection for send, wait, resume, and close without affecting
  a separately granted child-to-Main route.
- Child-to-Main close rejection while exact-turn `m` communication continues
  to work.
- A maximum-depth V1 child retaining `send_input` for an explicit `m` route
  while deeper lifecycle tools remain unavailable.
- Explicit user spawning or adoption beyond the autonomous model depth budget
  recording the actual graph depth while the resulting leaf remains able to
  communicate.
- Adoption of another root's Main assigning a generated destination nickname
  rather than importing the reserved `Main` identity.
- Competing adoption attempts yielding one exclusive owner.
- Cross-process writer conflict rejecting adoption without changing durable
  ownership, followed by successful adoption after the original writer closes.
- Ownership transfer invalidating target and descendant response observers before destination
  watcher registration, so former-root V1 recovery cannot attach to the adopted runtime.
- Generic V1 resume rebuilding the current durable owner's control plane when
  that root is unloaded instead of restoring the rollout's historical
  controller after transfer.
- A former owner failing stale same-root resume after transfer, then explicitly
  adopting the closed rollout back with its latest history and reserved alias.
- Canonical tool-call events retaining the resolved UUID without requiring a
  ref field.
- App-server alias list, update notification/mutation response, replay, and
  pagination returning the same committed ref.
- No synchronous app-server metadata read on the TUI rendering path.

## Open questions

- Should `spawn_agent` eventually omit the full UUID from its model-facing
  result while retaining it in canonical events?
