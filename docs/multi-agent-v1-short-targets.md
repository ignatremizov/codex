# Multi-agent v1 short targets

Status: design proposal

## Summary

Multi-agent v1 currently returns both a full thread UUID and a human-friendly
nickname from `spawn_agent`, but follow-up tools accept only the UUID. Repeating
UUIDs in `send_input`, `wait_agent`, `resume_agent`, and `close_agent` consumes
unnecessary model-context tokens and makes tool calls harder to read.

V1 should accept a spawned agent's nickname as a target alias while retaining
the full UUID as its canonical identity and compatibility fallback. Nicknames
are already persisted as agent metadata, but v1 does not currently reconstruct
its agent registry on cold root resume. The implementation must add a durable
alias index reconstructed from persisted spawn descendants, including closed
agents, so aliases remain valid without depending on live-agent registry
membership.

This proposal does not truncate UUIDs or introduce session-only numeric
references.

## Current behavior

`multi_agent_v1.spawn_agent` returns a result shaped like:

```json
{
  "agent_id": "019faa07-aa3d-78d3-9eca-66cd8626adad",
  "nickname": "Parfit"
}
```

The model can see both values, but the other v1 tools parse their targets
directly as `ThreadId` values:

```json
{"target":"019faa07-aa3d-78d3-9eca-66cd8626adad","message":"Continue"}
```

The nickname is presentation metadata only.

## Goals

- Reduce the model-visible token cost of routine multi-agent tool calls.
- Make targets easy for the model and humans to associate with an agent.
- Preserve stable targeting across cold resume.
- Keep full UUID targeting available for compatibility and recovery.
- Avoid changing canonical thread identity in core, rollouts, app-server APIs,
  notifications, logs, or TUI navigation.
- Produce explicit errors rather than selecting an arbitrary agent when a
  target is missing or ambiguous.

## Non-goals

- Replacing thread UUIDs as canonical identifiers.
- Changing app-server's client-facing thread identity.
- Deriving identity from a partial UUID.
- Adding ephemeral aliases that are valid only for one process lifetime.
- Making v1 adopt the complete v2 agent-path model.

## Proposed target syntax

The existing v1 `target` and `targets` fields should accept either:

1. A persisted agent nickname, preferred for model-authored calls.
2. A full thread UUID, retained as the canonical fallback.

Examples:

```json
{"target":"Parfit","message":"Continue"}
```

```json
{"targets":["Parfit","Socrates"],"timeout_ms":30000}
```

```json
{"target":"019faa07-aa3d-78d3-9eca-66cd8626adad"}
```

This applies consistently to:

- `send_input.target`
- `wait_agent.targets`
- `resume_agent.id`
- `close_agent.target`

Tool descriptions should tell the model to use the nickname returned by
`spawn_agent` and use the full UUID only when needed. The existing field names
must remain unchanged; in particular, nickname support broadens the accepted
values of `resume_agent.id` rather than renaming it to `target`.

## Resolution rules

Target resolution should be deterministic:

1. If the target parses as a full `ThreadId`, resolve it as that exact thread.
2. Otherwise, look for an exact nickname match in the root's durable agent
   alias index.
3. Return the matching thread UUID to the existing tool implementation.
4. If there is no match, return an error that identifies the unknown target.
5. If more than one match is ever present, return an ambiguity error listing
   the matching full UUIDs sorted by their string representation. Never pick
   one by registry iteration order.

Nickname matching should initially be exact and case-sensitive. Fuzzy or
case-insensitive matching would make collisions and model mistakes harder to
diagnose.

The resolver should be shared by all v1 follow-up tools so their accepted
syntax and error behavior cannot drift.

Product nicknames come from the defined nickname pool (or its ordinally
suffixed forms) and do not overlap the UUID namespace, so UUID-first resolution
is unambiguous.

## Cold resume

An in-memory spawn counter or live-agent-only alias table would be cleared on
cold resume while the old tool output could remain in model history. The model
could then issue a short target that no longer resolves or, worse, resolves to
a different agent.

Nicknames provide a durable source for reconstruction because they are already
stored with spawned thread metadata. However, current cold root resume restores
persisted agent metadata only for v2 roots; v1 roots do not yet reconstruct
their spawned-agent registry. Adding v1 reconstruction is required
implementation work, not an existing guarantee.

Cold resume must enumerate all persisted spawn descendants for the root,
including spawn edges marked closed, read their stored nickname metadata, and
rebuild a durable alias index before accepting nickname-targeted
`resume_agent`, `send_input`, `wait_agent`, or `close_agent` calls. The alias
index must be separate from live-agent accounting:

- Closing or unloading an agent removes its live runtime entry but retains its
  nickname-to-thread mapping.
- Closed nicknames stay reserved so a later spawn cannot reuse an alias still
  present in model history.
- `resume_agent.id` can resolve a closed thread by nickname and then use its
  canonical UUID for the existing resume path.
- Reconstructed aliases do not consume concurrent-agent capacity until their
  threads are actually resumed.

If the persisted spawn graph cannot be read, nickname targeting should remain
unavailable for that root and UUID targeting should continue to work. If one
descendant's metadata cannot be read, reconstruction should retain successfully
loaded aliases, warn with the affected thread UUID, and require UUID targeting
for the missing entry. It must not silently allocate a failed entry's nickname
to a new agent during that root session.

The UUID remains available when old rollouts lack nickname metadata or when
metadata reconstruction fails.

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
but their instability provides little benefit over persisted nicknames.

## Why not ephemeral spawn references

Sequential references such as `"1"` or `"a"` are compact and clear within one
live process, but they are unsafe unless the reference is persisted as part of
thread metadata. Reconstructing references from current registry order is not
sufficient because load order can change across resume.

A durable spawn ordinal could be designed later, but it would require:

- A new persisted metadata field.
- Backfill behavior for older rollouts.
- App-server and replay decisions about whether the ordinal is public.
- Collision and monotonicity rules across forks, resumed roots, and imported
  threads.

That additional identity system is not justified while persisted nicknames
provide most of the token savings.

## Spawn result

The compatible first step is to continue returning both `agent_id` and
`nickname`, while updating tool guidance to prefer the nickname.

Omitting `agent_id` from model-visible output would save more tokens, but it is
a separate compatibility decision. Existing prompts, tests, and consumers may
expect the field, and the UUID remains useful for diagnosing ambiguity or
missing metadata. Any later omission should preserve the UUID in canonical
events and external APIs.

## Compatibility

- Existing UUID calls continue to work unchanged.
- Existing rollouts remain replayable.
- The wire shape of `spawn_agent` need not change for the initial
  implementation.
- `target` and `targets` are already semantically neutral names, so accepting a
  nickname broadens their input domain without adding parallel parameters.
- `resume_agent` keeps its existing `id` field and broadens that field to accept
  either a nickname or full UUID.
- Canonical collab tool-call items should continue recording resolved thread
  UUIDs and agent metadata, not the user/model-authored alias.

## Error behavior

Suggested errors:

```text
agent target "Parfit" was not found
```

```text
agent target "Parfit" is ambiguous; use a full agent id: <id-1>, <id-2>
```

Malformed UUID-like strings should follow the same target-resolution rules
rather than receiving a special partial-UUID interpretation.

## Coverage

Implementation should cover:

- Each v1 follow-up tool resolving an exact nickname.
- Full UUID behavior remaining unchanged.
- `resume_agent.id` accepting nicknames without adding or requiring a
  `resume_agent.target` field.
- Unknown nicknames returning a model-visible error.
- Ambiguous nicknames failing with UUIDs in deterministic sorted order.
- Nickname resolution after cold root resume.
- Nickname resolution for a closed descendant after cold root resume.
- Closed or unloaded aliases not counting against concurrent live-agent
  capacity.
- A later spawn not reusing a closed descendant's reserved nickname.
- Partial reconstruction failure preserving UUID fallback without reusing
  unresolved aliases.
- Older threads without nickname metadata remaining addressable by UUID.
- Canonical tool-call events storing the resolved UUID and restored nickname.
- No synchronous app-server metadata read on the TUI rendering path.

## Open questions

- Should `spawn_agent` eventually omit the full UUID from its model-facing
  result while retaining it in canonical events?
- Should errors expose all matching UUIDs or a shorter diagnostic plus a tool
  that lists agents?
- Is a future durable numeric spawn ordinal worth adding after nickname
  targeting is measured in production?
