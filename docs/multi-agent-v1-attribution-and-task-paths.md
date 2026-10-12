# V1 attributed messaging and task-path discovery

Status: typed V1 attribution, spawn/adoption task labels, opt-in root-controlled discovery, and live-only audit presentation are implemented. Same-owned-tree sends are always available; directed/subtree permission controls and their proposed config defaults are retired. Later task-label editing remains a proposal. Source integration is not compilation, test, generated-schema, or CI qualification.

Related contracts:

- [V1 response observation](multi-agent-v1-response-observation.md)
- [V1 short targets](multi-agent-v1-short-targets.md)
- [TUI agent control](tui-agent-control.md)

## Purpose

Support supervisors coordinating frontend/backend coders and reviewers that communicate mid-task. Recipients must distinguish agent input from actual user input and understand which agent owns which responsibility. Preserve full plaintext auditability without duplicating V2's separate message and follow-up-task APIs.

This upgrades V1; it does not introduce a third multi-agent version.

## Identity and targeting

Keep these concepts separate:

- **Thread UUID:** canonical durable identity, including in rollouts and external APIs.
- **Numeric ref:** existing stable root-scoped short target, preferred for routine calls.
- **Nickname and role:** human-readable identity and configured behavior.
- **Task path:** optional semantic label describing the assignment, such as `/root/backend/auth`.

The latest prompt does not replace the task path. A follow-up like “run tests again” must not change the agent's assignment label.

Proposed spawn field:

```json
{"task":"backend/auth","message":"Implement the authentication API described in the spec."}
```

`task` is optional. Agents without it retain UUID/ref/nickname targeting. Use the supplied path directly; no hidden model request is needed to generate a description.

### Canonical paths

Retain the `/root/` prefix: when Main spawns the example above, it becomes `/root/backend/auth`. Relative paths resolve from the calling agent's task path: `/root/backend` spawning `task:"review"` creates `/root/backend/review`. Absolute `/root/...` paths address the root-scoped namespace directly. Full paths remain available in discovery, message envelopes, detailed inspection, and audit records.

Paths are root-scoped labels, not filesystem locations or lifecycle edges. Renaming a task does not reparent a thread, change its UUID/ref, transfer ownership, or grant permissions. For example, naming a reviewer `/root/backend/auth/review` does not make it a child of the authentication coder.

Snapshot the sender's nickname, task path, and ref at send time alongside canonical UUID identity. Historical envelopes retain that attribution after a rename; do not retroactively relabel old messages. Discovery and targeting use current labels. An old path in a message is attribution evidence, not a guarantee that the path remains a valid selector; canonical identity remains available for resolution.

Separate roots can each contain `/root/backend/auth`; canonical identity still includes the owning root UUID and target thread UUID. A path alone is not a cross-root identifier. Adoption and cold restoration must use the existing ownership rules, not inferred path ancestry.

Extend the shared target resolver so a task path can target its unique agent wherever existing V1 selectors are accepted. Preserve UUID/ref/nickname behavior.

### Path and lookup rules

- Resolve relative spawn labels, task selectors, and directory prefixes against the calling agent's task path; use `/root` for Main or a caller without a task path. Thus `backend/auth` and `/root/backend/auth` are equivalent from Main, not from every child. Use absolute paths or refs for peers outside the caller's path prefix. TUI commands use the issuing thread as the caller.
- Reserve `/root` for Main. Child paths contain one or more nonempty segments; reject repeated/trailing slashes, `.`/`..` segments, whitespace, control characters, and backslashes. Preserve case and Unicode; do not silently slugify or interpret paths as filesystem locations.
- Add `task:<path>` as the explicit selector, for example `task:backend/auth`. Preserve existing forced UUID/ref/nickname selectors and their precedence. An unprefixed canonical path resolves as a task path; other unprefixed selectors retain existing identity lookup first, then exact task-label lookup.
- Task paths are unique within the currently owned graph, including closed members. A spawn requesting an existing path fails before publishing a new child and reports the existing agent's ref, UUID, and status. If closed, suggest resuming it; otherwise suggest using the existing agent. Creating another agent requires a different explicit task path. Do not silently resume, replace, or generate a path suffix.
- Resolve to canonical thread identity before invoking the existing lifecycle/permission operation. A path match does not make a closed target live, adopt a foreign thread, or grant communication.

Persist task labels with existing root-scoped agent metadata rather than building a second graph. Enforce uniqueness atomically with the existing metadata allocation/publication boundary so concurrent spawns cannot claim the same path. Closing and resuming the same member retains its label. Transferred-out members are historical records, not selectable members of the former graph.

Cross-root adoption is the suffixing exception: preserve an imported path when available; otherwise append the first available numeric suffix starting at `-2`, for example `/root/backend/auth-2`. Allocate the destination paths for the adopted subtree together with ownership transfer, checking existing members and the other imported paths. Preserve matching path prefixes when remapping descendants, but do not infer lifecycle ancestry from path prefixes. Report the resulting old-to-new path mapping, keep UUIDs unchanged, and retain historical send-time attribution. Never rename an existing destination member to make room. Ordinary spawn collisions still fail rather than silently suffixing.

Allow an optional `task` on cross-root `resume_agent` adoption and `task:<path>` on the equivalent TUI resume command to assign the imported agent's new responsibility, with the same caller-relative resolution. Without an explicit assignment, an adopted former Main loses its reserved `/root` label and remains addressable by nickname/ref/UUID; do not invent `/root/imported` or a model-generated assignment. Preserve existing non-root labels subject to collision suffixing. Report assigned, cleared, and remapped labels explicitly. Reject `task` on already-owned resume rather than introducing an implicit rename operation.

Do not add task-label tombstones or permanent path reservations: refs and UUIDs provide stable addressing; paths are mutable assignment labels. If a label changes, old envelopes keep the send-time snapshot and current lookup uses the new label. An old path may later describe another agent, so callers needing stable identity must use the ref or UUID. Never replay historical tool calls merely to reconstruct targeting state.

The first implementation needs spawn-time labels and restoration, not a separate rename tool. A later label-edit operation must preserve the identity and audit rules above; it must not reuse `/rename`, which names the conversation.

## Attributed agent input

Model-authored delegation and peer messages should use typed agent-message envelopes instead of appearing to be human-authored user input. This includes ordinary parent-to-child sends, not only reverse routes or peer sends.

Illustrative TUI rendering, including sender model and reasoning:

```text
Pascal [coder] /root/backend/auth (3) (gpt-6-astra low) sends:
  └ API now requires document_id. Update the client.
```

Model-visible context uses a compact `<agent_message>` envelope, omitting role, model/reasoning, `sends`, and the TUI-only `  └` decoration:

```text
<agent_message>
{"ref":"3","message":"API now requires document_id. Update the client."}
</agent_message>
```

Routine V1 commentary and completion envelopes likewise project only the receiver-local ref and message or status. When that receiver has no current mapping for the sender, the projection retains `agent_id` instead of borrowing another root's ref. Turn and item IDs remain internal delivery identities rather than repeated model-visible fields.

Projection applies to disposable sampling, local compaction, and V2 remote-compaction requests. Canonical envelopes and typed presentation retain the full UUID and send-time attribution. Fresh attributed user-role input is marked at its trusted recording boundary using the existing content-kind metadata; marker-looking ordinary user text is not rewritten. Unsupported legacy envelopes remain readable without inventing identity. Retaining canonical UUIDs lets later requests safely project against changed receiver mappings after transfer or adoption.

Spawn and resume results hydrate the ref, nickname, and task path together. Receiver-scoped identity context is refreshed for model requests and after compaction, including closed but addressable members; it is not conditional on environment-context display. A namespace containing only its root omits both hydration and compact-ref projection; once another owned member exists, Main is eligible alongside that member. The discovery summary has a 2,048 approximate-token budget, including framing and an omission notice. It prioritizes active aliases in ascending ref order, then closed aliases in descending durable ref order; newer refs are not a measure of last closure time. Entries fit whole or are skipped, never shortened. The projection map contains exactly the advertised subset, with UUID fallback for omissions and an omission notice pointing to existing agent discovery. This budget does not cap labels, task assignment, messages, spawn/resume results, or canonical history. Current labels support targeting, while the durable audit retains historical send-time labels.

Incremental identity updates have a separate 16,384-byte rendered-fragment ceiling. Replaying individually bounded deltas must not accumulate a larger identity map. An oversized or unusable retained chain, or a removal/update delta that exceeds the ceiling, falls back to the complete current bounded authority without rewriting earlier history. These limits do not truncate agent messages or their canonical evidence.

A stream retry refreshes the identity snapshot after draining completed tools from the failed attempt. Its rebuilt history, identity hydration, agent envelopes, and mailbox projections all use that one refreshed snapshot; a successful spawn receipt must not be paired with the pre-spawn mapping. This does not rewrite the stored history or re-run completed lifecycle tools.

The example is illustrative, not permission to concatenate arbitrary text into delimiters. Use reversible escaping or structured serialization for header values and payload text so embedded `<agent_message>`/`</agent_message>` strings, newlines in labels, or fabricated sender headers remain data rather than additional envelope structure. Preserve the original payload in audit storage and recover it exactly for human presentation. This prevents structural ambiguity; it is not a claim that serialization alone prevents model confusion or prompt injection.

Once a complete mapping is retained, ordinary updates append a JSON-only object
inside `<agent_identity_context>`, without repeating instructional prose. Its
`patch` array contains a `ref` and only changed fields for each existing identity;
omitted fields retain their values, while explicit `null` clears a nickname or
task path. For example, a lifecycle-only update is
`{"patch":[{"ref":"2","state":"closed"}]}`. New identities appear as complete
entries in `upsert`; removals appear in `removed_refs`. Empty operation arrays are
omitted. Existing instruction-prefixed full mappings and historical complete-entry
`upsert` deltas remain readable without rewriting them. A patch requires an existing
baseline for its ref; malformed or conflicting operations reset to a full mapping.
Unchanged mappings add nothing; prior prompt content and
annotations stay intact. Missing or malformed bases, and changes to the omission
boundary, require a fresh full mapping. Empty authority explicitly clears earlier
refs. World-state checkpoints still retain full bounded snapshots, so missing
hydration after compaction or resume does not depend on keeping every old delta.
Neither these discovery deltas nor compact mailbox inventories grant messaging
permissions or change canonical sender/receiver attribution.

Core supplies trusted sender/recipient identity; names embedded in payload text do not establish attribution. Preserve the payload completely.

Array-target `send_input` admissions also retain the sender's shared tool-call ID as an optional
typed batch correlation. Main receives one live-only consolidated batch presentation with the
individual recipient outcomes and handling policy; it does not receive one duplicated full payload
per recipient. Singleton sends and older records have no batch correlation and retain their
existing presentation. The consolidated Main copy is not persisted or added to model context;
the sender's canonical lifecycle and each recipient's durable attributed input remain authoritative
for replay.

Human `/agent <target> <prompt>` input remains an ordinary `UserMessage` in the target thread, with the same user styling and input semantics as typing directly in that thread. Do not wrap it in `<agent_message>` or attribute it to the agent whose TUI the user happened to use. Preserve that distinction in model context, transcript rendering, persisted rollout history, and resumed/reconstructed history. User-authored spawn and queued prompts follow the same authorship rule; queueing changes admission timing, not authorship. Keep the issuing thread's user-control audit presentation separate from the target's user message.

Text-oriented consumers must retain this distinction too. Thread search indexes received agent-input payloads even when they are not the turn's final message. `codex exec --json` emits received input as `agent_input` with `sender_thread_id`, `recipient_thread_id`, and `text`, distinct from the executing agent's `agent_message`; receiving input must not replace the final response or `--output-last-message` contents. Text exports render media as attachment markers while the canonical typed history retains the attachments.

Main's live delivery-acknowledgment copies are TUI presentation, not a second assistant answer. Human and JSONL exec output omit those receipt copies and exclude them from final-message fallback and `--output-last-message`, including when a refreshed turn contains a receipt after Main's actual answer. The communicating threads retain their canonical history independently.

Do not add an arbitrary per-message token ceiling or truncate complete agent payloads to satisfy a generic review threshold. Full agent messages are an intentional fork-policy exception; provider context limits and explicit user configuration remain separate concerns.

Reuse or extend the existing `AgentMessage` representation and shared projection machinery where appropriate. Do not select a wire representation solely because its name fits: verify Responses API requirements, item-ID prefixes, persistence, and model serialization first.

Attribution identifies the sender. Any two agents in the same owned tree may use `send_input`;
there is no separate messaging-permission route to configure.

## Same-tree messaging and compatibility

Parents, siblings, and descendants can send directly without grants or per-agent send settings. Immediate input steers an active recipient or starts an idle turn. A single originating sender turn can start later idle recipient turns; there is no one-idle-wake cap. `w:q` retains a distinct FIFO turn and next-source-turn response boundary. Only descendants may be interrupted; sending does not grant broader lifecycle control.

The `m` flag, `/agent sends`, and its picker controls are removed. The app-server `replyRoute` and `subtreeMessaging` action variants reject as obsolete before mutating runtime state. Existing wire fields, SQL settings, and typed history remain readable but do not restrict admission. Old rollouts are not rewritten. Classified route and permission notices remain omitted from model requests; ordinary quoted text and canonical sender attribution are preserved.

Ownership and runtime identity still matter. Direct and queued admission validate the exact live endpoints within one owned tree. Close and ownership transfer retain their lifecycle/source-admission gates, cancellation, and accepted-work draining. Cold resume and fork do not restore unadmitted queues or old response subscriptions. No special messaging-policy handoff is needed for live revert.

Mailbox acceptance still requires durable same-root identity and preserves invocation idempotency and attribution. Consumption of accepted pending mail no longer consults old send settings. Consumed and rejected rows remain terminal; removing permission checks does not requeue them.

## Shared targets: observation versus addressed replies

`w:c` observes the target's first subsequent commentary, and `w:f` observes its turn completion. Neither correlates that output to one sender's instruction. If backend API and backend logic agents both steer a schema agent during the same turn, the observed commentary or final response may address either request or both.

Use workflow guidance to choose observation versus an addressed reply:

- Supervisors use `w:cf` when acknowledgement and completion observation are useful, or ask workers for explicit mid-turn updates through `send_input`.
- Peers normally coordinate and answer one another with `send_input(..., w:"x")`, preserving attribution without subscribing to the recipient's commentary or final response.
- Workers send requested mid-turn updates to their supervisor with `w:"x"` so the supervisor's later completion does not enter the worker's task context.
- `w:q` separates input into queued turns when serialized work is useful; it is not reply addressing.

For example, within one owned tree:

```text
backend-2 → backend-1: Confirm schema for endpoint A. Reply via send_input.
backend-3 → backend-1: Need nullability decision for field B. Reply via send_input.
backend-1 → backend-2: Endpoint A uses …
backend-1 → backend-3: Field B is nullable …
```

The supervisor or workflow skill should establish these conventions when assigning collaborating agents. Tool guidance should explain the underlying distinction once: `c/f` observe shared target output; `send_input` addresses a recipient. These are defaults for coordination, not restrictions on valid flag combinations.

Current supervisor skills should assume that same-owned-tree messaging is available without a
separate permission-control step.

If a sender requests `c` and the recipient emits commentary followed by an explicit `send_input` reply, both are legitimate deliveries. Do not suppress one as a duplicate merely because their meaning overlaps; existing exact-event deduplication remains separate.

Do not parse model-authored `Reply 2:` prefixes as routing instructions or synthesize “no reply made—ask again” messages, which could cause retry loops. Guaranteed per-message reply correlation, if needed later, should use structured message IDs and `reply_to`; it is outside this proposal.

## Discovery and presentation

Discovery answers “who owns this work?” within the owned tree. It is independent of always-on same-tree messaging and does not grant lifecycle control.

Illustrative directory:

```text
3 Pascal [coder] /root/backend/auth
4 Curie [coder] /root/frontend
5 Ohm [reviewer] /root/review/contracts
```

Show task path alongside existing role, ref, nickname, status, model, and reasoning metadata. Keep latest sent task, latest own response, and latest received message separate.

Provide a user-controlled, default-disabled read-only V1 `list_agents` directory backed by the existing root-scoped alias/graph query, not a rollout scan or a second agent registry. Enable it with `[tools.list_agents] enabled = true`; omission or an empty table leaves it disabled. The owning root's setting applies to current and future members, including depth-limited leaves, without exposing additional lifecycle tools. Reuse the existing paginated `agentAlias/list` data source and extend its projection with task labels; TUI and model lookup must agree on identities. The V1 tool accepts an optional exact-or-subtree `path_prefix`, status filter, cursor, and limit, returns entries in stable ref order with a next cursor, and lists only members of the caller's owned graph. Default to loaded members, including running agents and loaded agents awaiting further input; callers can request running-only results, a specific lifecycle status, closed members, or all owned members explicitly. Apply filters before pagination. Excluding a closed member from default results does not release its task path. It neither resumes agents nor creates response subscriptions. Keep routine entries to ref, nickname, role, task path, and status; canonical UUID remains available in the result. Model/reasoning and full task/message payloads need not be repeated in this directory.

V2 already has `list_agents`; reuse that familiar name and applicable shared machinery without importing V2 task/mailbox lifecycle semantics or changing V2's existing availability in this pass. Its unfiltered listing covers the root control's live graph, not merely the caller's siblings or descendants; relative `path_prefix` values resolve against the caller's path. Keep the same unfiltered graph scope for V1, with the explicit status/pagination contract above.

V1 directory discovery requires explicit user enablement and supports explicit user disablement. When disabled, omit the model-facing tool and reject stale or previously discovered invocations at execution. Skills and supervisor prose cannot enable it. Disabling discovery does not retract already seen identities or prevent sends to known same-tree refs/UUIDs. Human `/agent` inspection and the app-server alias-list API remain available independently.

When enabled for the graph, directory discovery applies to its current and future members, including leaves that cannot spawn. Do not infer enablement from task-path use, model metadata, or role selection. Reuse existing user-controlled tool-availability configuration where possible; a live user override, if exposed, must follow the same admission gate and must not be restored merely from historical tool calls.

Reuse existing bounded identity/context machinery for attributed senders, avoiding repeated UUID lists and repeated tool instructions. This hydration is distinct from directory enumeration; do not inject the whole directory as a substitute for disabled `list_agents`.

Preserve sender→recipient presentation in Main for peer communication, marked presentation-only relative to Main's model. Such display copies must not become Main model inputs or accidental subscriptions. Live, resumed, paginated, non-paginated, Review, and Full transcript projections must retain attribution and full auditable payloads under their existing presentation contracts.

Main also receives a durable, presentation-only conclusion for subsequent live V1 child turns, including turns initiated by a peer. These conclusions use the canonical completion identity and the `NotVisible` marker (`○`): they do not add model context, wake Main, replace Main's own final response, or subscribe Main to future replies. Unlike live peer-input display copies, conclusion rows are stored for transcript replay after canonical acknowledgement. Cold resume does not regenerate audits from old turns or restore messaging grants.

An explicit Main final-response observation of the same target turn (`f`, passive, or `x`) owns its existing canonical delivery instead of producing an additional oversight row. An active `wait_agent` can claim the same terminal presentation through normal wait arbitration. A policy on an earlier turn does not suppress a later peer-turn audit. The child must explicitly message or wake Main if model attention is needed; Main may then explicitly observe subsequent work.

## V1/V2 interoperability boundary

Share identity, attribution envelopes, and transcript projection where semantics match. Retain V1's single `send_input` plus `w` contract. Do not import V2's task/mailbox lifecycle merely to obtain readable paths or sender attribution.

A later V2 `send_message`/`followup_task` consolidation can build on this common representation. Actual cross-version routing still requires an explicit lifecycle/delivery mapping; common paths or envelopes alone do not prove transport interoperability.

## Implementation sequence

1. Add trusted attribution for V1 agent-originated input without permission-change context.
2. Add optional task paths, directory presentation, and shared targeting.
3. Reconcile reusable envelope/projection code with V2; keep tool/lifecycle consolidation separate.

Keep each change reviewable and attach schema, documentation, and coverage to its owning feature.

### Spawn configuration regression guard

Task paths and attributed input extend the existing spawn flow; they must not introduce a reduced configuration path for nested agents. Preserve role selection, model/reasoning overrides, service-tier handling, history-fork controls, and `w` semantics for both Main-to-child and child-to-grandchild spawns, through model tools and user TUI dispatch.

Preserve spawn model/reasoning precedence: inherited parent settings → configured child defaults → selected role defaults → explicit request overrides. Explicit model/reasoning values override role defaults. A model-only override at any layer clears lower-precedence reasoning before resolving the selected model's catalog-default effort; an explicit effort is validated against the final model. Role base/developer instructions and authorized role configuration must still use the existing shared application path. Do not reset persisted resolved model/reasoning to role defaults during resume.

Task labels do not authorize history inheritance, change its default of no inherited conversation, couple full-history forks to the parent model/reasoning, bypass depth or permission checks, or introduce model-catalog multi-agent-version gates. Retain existing service-tier precedence rather than assuming every configuration field follows model/reasoning precedence.

## Required coverage

- Parent, sibling, and child sends identify the real sender and preserve exact payloads.
- Human `/agent` prompts, including spawn and queued prompts, remain ordinary target-thread user messages with normal user styling and semantics in live, resumed, paginated, and non-paginated history; only model-authored sends receive agent attribution. Issuing-thread audit records do not change target authorship.
- Payload text cannot forge trusted attribution.
- Embedded envelope delimiters, fabricated headers, and header-value newlines remain payload data under the selected serialization; audit payloads round-trip without loss.
- Knowing a foreign-tree ref/path does not confer ownership or authorize cross-tree sends.
- Parent, sibling, and upward sends work without grants and select commentary/final observations independently.
- Historical settings do not alter current admission or install subscriptions.
- Future children receive bounded current identities without inheriting parent conversation history.
- Task targeting agrees across tools and TUI; ordinary duplicate-path spawns fail for both live and closed members, including concurrent spawns, without publishing a conflicting agent.
- Nested spawns and relative selectors/prefixes resolve against the caller's task path; absolute paths remain root-scoped, callers without a task path use `/root`, and unfiltered directory discovery is not restricted to siblings or descendants.
- Verify actual V1/V2 child and grandchild request payloads with task labels present and absent: role defaults, conflicting configured child defaults, explicit model/effort overrides, model-only reasoning reset, and role base/developer instructions must match the existing spawn contract. Include user TUI dispatch and close/unload/resume preservation of resolved settings.
- Cover default no-history spawns and explicitly authorized history forks with independent model/reasoning overrides; preserve service-tier and `w` behavior at nested depth without weakening existing admission checks.
- Separate roots can reuse paths; adoption allocates collision-free suffixed paths for the imported subtree atomically, reports the mapping, and preserves UUIDs and historical attribution.
- Directory filtering defaults to loaded members, supports running/status/closed/all selection before pagination, and does not release paths hidden by a filter.
- V1 `list_agents` is absent and rejects stale calls by default; explicit user enable/disable affects current/future graph members independently of messaging, while human inspection remains available.
- Rename, close/resume, adoption, and history fork preserve canonical identity without confusing separate roots with identical paths.
- Renames preserve send-time attribution while directory lookup uses current labels.
- Source close before admission rejects queued work; accepted source work retains exact-incarnation draining.
- Repeated direct and queued peer sends from one originating turn can start later idle recipient turns.
- Non-paginated rollback and paginated revert preserve attribution through runtime replacement and inherited history without restoring retired messaging controls.

- Compaction/rollback and both history modes preserve attribution and correct effective context.
- Main's peer presentation copies remain absent from Main model input.
- JSON/TypeScript/generated exports and Responses API item identities match the chosen typed representation; include request-payload integration tests and TUI snapshots.
