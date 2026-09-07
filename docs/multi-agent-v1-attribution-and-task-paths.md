# V1 attributed messaging and task-path discovery

Status: proposed extension. Examples below describe the target contract, not additional tools or fields already available.

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
Pascal /root/backend/auth (3):
API now requires document_id. Update the client.
</agent_message>
```

This is the proposed compact rendering. The existing attributed-input fragment already uses `<agent_message>` markers, with a JSON body containing identity, turn ID, and message. Keep structured audit identity separate from the concise model-visible presentation.

The example is illustrative, not permission to concatenate arbitrary text into delimiters. Use reversible escaping or structured serialization for header values and payload text so embedded `<agent_message>`/`</agent_message>` strings, newlines in labels, or fabricated sender headers remain data rather than additional envelope structure. Preserve the original payload in audit storage and recover it exactly for human presentation. This prevents structural ambiguity; it is not a claim that serialization alone prevents model confusion or prompt injection.

Core supplies trusted sender/recipient identity; names embedded in payload text do not establish attribution. Preserve the payload completely.

Human `/agent <target> <prompt>` input remains an ordinary `UserMessage` in the target thread, with the same user styling and input semantics as typing directly in that thread. Do not wrap it in `<agent_message>` or attribute it to the agent whose TUI the user happened to use. Preserve that distinction in model context, transcript rendering, persisted rollout history, and resumed/reconstructed history. User-authored spawn and queued prompts follow the same authorship rule; queueing changes admission timing, not authorship. Keep the issuing thread's user-control audit presentation separate from the target's user message.

Do not add an arbitrary per-message token ceiling or truncate complete agent payloads to satisfy a generic review threshold. Full agent messages are an intentional fork-policy exception; provider context limits and explicit user configuration remain separate concerns.

Reuse or extend the existing `AgentMessage` representation and shared projection machinery where appropriate. Do not select a wire representation solely because its name fits: verify Responses API requirements, item-ID prefixes, persistence, and model serialization first.

Attribution identifies the sender; it does **not** authorize a reply. A child that learns Main's ref or UUID still needs an applicable route to call `send_input` upward.

## Permission and observation remain independent

Keep `send_input` as the single V1 messaging operation and preserve existing `w` semantics. Message attribution must not silently add a completion subscription or wake the sender.

Normal parent-to-descendant task dispatch remains permitted. User controls gate reverse and peer communication:

```text
/agent sends <sender> [to <recipient>] enable|disable
/agent sends all enable|disable
```

Omitted recipient means the issuing thread. An explicit pair grant is directional. `all` covers the issuing supervisor and current/future descendants, not outside ancestors or siblings. Explicit pair settings override subtree defaults; the nearest common supervisor with a configured default determines inherited permission.

Downward supervisor dispatch is outside these permission disables. `/agent sends all disable` disables applicable reverse/peer routes, not parent-to-descendant task assignment. Reject an explicit downward disable with a clear explanation instead of recording or displaying a disabled route that runtime dispatch ignores.

User TUI grants persist across live turns, not process shutdown, cold resume, or history fork. Ownership transfer does not export live grants. Retained historical instructions are audit evidence, not executable permission state.

### Layer boundaries and policy resolution

Separate permission, workflow instructions, and response observation. Only effective runtime permission authorizes a send; text describing a workflow or naming an agent cannot grant access.

| Layer | Responsibility |
|---|---|
| Global config | Default communication policy for newly initialized graphs. |
| Agent role config | Default for the selected supervisor role's subtree, including future children. It does not grant access outside that subtree. |
| Tool schema | Explain `send_input`, targeting, `w`, and permission errors once. Do not embed dynamic directories or per-agent permission lists in the schema. |
| Skill file | Guide coordination, reporting, and escalation. Skill text does not grant permissions. |
| Supervisor message | Assign responsibilities and request coordination. Message text does not grant permissions; existing `w:m` remains an explicit exact-turn reverse-route grant. |
| User TUI | Authoritative live override through subtree defaults and directed exceptions; update runtime policy and notify affected senders without waking idle threads. |

For the planned config/role surface, resolve defaults when initializing a graph or role subtree, rather than rereading configuration on every send. A selected role supplies its subtree default over the global fallback. Explicit user TUI settings override those configured defaults. Within live user settings, directed pair exceptions override subtree defaults; otherwise use the nearest common supervisor's applicable subtree setting. A user disable cannot be bypassed by `w:m`, a skill, or a supervisor's prose instruction.

Config and role fields remain follow-up work; this spec does not introduce TOML names or require new configuration plumbing in the initial implementation. Cold resume and history fork initialize from applicable configured defaults once that surface exists, not replayed TUI grants. That is fresh policy initialization, not restoration of live subscriptions or automatic recreation of subagents.

Keep one authoritative runtime policy representation: subtree defaults plus sparse directed exceptions, not a separately maintained grant for every possible pair. Admission, context updates, and `/agent` presentation must use the same effective-policy resolver. Cached projections are not another authority.

Show policy provenance in the picker, for example `enabled · inherited from Main` or `disabled · user override`. Keep permission-change audit records distinct from message-delivery records. Main's sender→recipient peer-message copies remain presentation-only and must not become model input.

Enabling a subtree informs affected loaded members once; future members receive current applicable policy at startup. Disabling rejects newly unauthorized sends, while already admitted work continues. Neither operation starts work merely to announce policy. Explicit pair exceptions remain effective and must not be obscured by a blanket subtree notice.

Admission belongs to an individual input, not a permission grant. Establishing `w:m` does not pre-admit future reverse messages: each later `send_input` checks current effective permission, including user revocation. “Already admitted work” means an actual accepted input, not an unused route or wake reservation. A queued peer message must pass the current permission check at target-turn admission; mere enqueueing does not grandfather permission.

Keep `w:c/f/x/q` response/input handling separate from messaging permission. Keep peer discovery separate from policy changes: do not resend the directory whenever the user changes a permission.

### Rollback, paginated revert, and live-session lifecycle

- **Non-paginated rollback and paginated `thread/revert`:** changing the conversation boundary of the same logical thread does not undo live permission settings. Retain their audit provenance independently of removed conversational work, and reconcile reconstructed model context with current effective policy so an old grant or missing revocation cannot misrepresent permission.
- **Close:** ends directed live grants incident to the closed thread and any subtree setting owned by that thread. It does not remove a still-live ancestor supervisor's subtree default.
- **Warm resume:** does not restore the closed thread's ended grants from history. A still-live supervisor's default can independently apply to the resumed member through current membership; present the resulting effective policy without reviving old pair exceptions or completion subscriptions.
- **Cold resume/history fork:** initialize from applicable configured defaults when supported, never replay historical TUI grants as authority. Do not recreate subagents or subscriptions merely to restore a directory.
- **Ownership transfer:** revoke former-owner grants. Resolve policy in the destination graph independently; historical attribution and permission-change records remain audit evidence.

Keep runtime policy, audit records, and model-visible current-policy context distinct across these boundaries. Reconcile stale context without waking idle agents solely to announce the reconciliation.

Paginated revert can replace the underlying runtime while retaining the logical thread UUID. Treat that replacement as part of the explicit live revert operation, not as permission restoration from rollout history or a user close/cold resume. Carry current permission state through the operation's existing lifecycle boundary and reconcile it before accepting new sends. Do not recreate completion subscriptions merely because messaging permissions survive.

If the user instead forks at an earlier turn, the new thread follows history-fork rules and receives no historical live grants. Rollback/revert must also discard queued inputs or pending deliveries whose originating work was removed, under their existing turn-boundary rules; retaining a permission does not retain an obsolete message. Cover permission changes both before and after the selected boundary, including boundaries around compaction and inherited paginated history.

### Model-visible permission updates

When a user changes effective permission, deliver a concise developer-context update to the affected sender without waking an idle thread merely to announce the change.

A directed command updates one destination. For `/agent sends 2 to 3 enable`, agent 2 receives:

```text
User enabled send_input to Pascal (3).
```

Use `disabled` for the corresponding disable command. Do not list unrelated agents or imply batch command syntax.

For `/agent sends all enable`, summarize the subtree scope:

```text
User enabled send_input within Main's subtree, including sibling communication and future agents.
```

For a nested supervisor, replace `Main` with its ref or canonical task path, for example `ref 2's subtree`. Do not say “your subtree” in a notice distributed to children: each recipient would interpret that scope differently. Explicit pair exceptions must remain clear; do not describe an inherited grant as overriding them.

Permission changes affect `send_input`, not commentary/final-response delivery. Explain that distinction in tool guidance rather than repeating “Response subscriptions unchanged” in each context update. Do not add “No new task,” which could be read as an instruction to stop ongoing work. Do not describe disabled `send_input` as “on turn completion”: commentary/final delivery is a separate observation policy and may be passive, waking, or presentation-only.

Avoid repeated identical updates. Future children receive current applicable permission state once at startup. Preserve changes in the audit history; compaction should carry current effective state rather than accumulating obsolete permission instructions. Cold resume must not restore permissions simply because an old grant appears in history.

### Shared targets: observation versus addressed replies

`w:c` observes the target's first subsequent commentary, and `w:f` observes its turn completion. Neither correlates that output to one sender's instruction. If backend API and backend logic agents both steer a schema agent during the same turn, the observed commentary or final response may address either request or both.

Use workflow guidance rather than additional permission modes:

- Supervisors use `w:cf` when acknowledgement and completion observation are useful, or ask workers for explicit mid-turn updates through `send_input`.
- Peers normally coordinate and answer one another with `send_input(..., w:"x")`, preserving attribution without subscribing to the recipient's commentary or final response.
- Workers send requested mid-turn updates to their supervisor with `w:"x"` so the supervisor's later completion does not enter the worker's task context.
- `w:q` separates input into queued turns when serialized work is useful; it is not reply addressing.

For example, with the necessary directed permissions enabled:

```text
backend-2 → backend-1: Confirm schema for endpoint A. Reply via send_input.
backend-3 → backend-1: Need nullability decision for field B. Reply via send_input.
backend-1 → backend-2: Endpoint A uses …
backend-1 → backend-3: Field B is nullable …
```

The supervisor or workflow skill should establish these conventions when assigning collaborating agents. Tool guidance should explain the underlying distinction once: `c/f` observe shared target output; `send_input` addresses a recipient. These are defaults for coordination, not restrictions on valid flag combinations.

Follow-up: once `/agent sends all enable` is available, update the existing supervisor skills in the separately managed skills repository with these conventions. Skills must not assume that messaging permission has been granted.

If a sender requests `c` and the recipient emits commentary followed by an explicit `send_input` reply, both are legitimate deliveries. Do not suppress one as a duplicate merely because their meaning overlaps; existing exact-event deduplication remains separate.

`/agent sends` must not disable `c/f`. Do not parse model-authored `Reply 2:` prefixes as routing instructions or synthesize “no reply made—ask again” messages, which could cause retry loops. Guaranteed per-message reply correlation, if needed later, should use structured message IDs and `reply_to`; it is outside this proposal.

## Discovery and presentation

Discovery answers “who owns this work?”; permission answers “may I message them?” Keep both visible without repeating the complete directory on every permission change.

Illustrative directory:

```text
3 Pascal [coder] /root/backend/auth
4 Curie [coder] /root/frontend
5 Ohm [reviewer] /root/review/contracts
```

Show task path alongside existing role, ref, nickname, status, model, and reasoning metadata. Keep latest sent task, latest own response, and latest received message separate.

Provide a user-controlled, default-disabled read-only V1 `list_agents` directory backed by the existing root-scoped alias/graph query, not a rollout scan or a second agent registry. Reuse the existing paginated `agentAlias/list` data source and extend its projection with task labels; TUI and model lookup must agree on identities. The V1 tool accepts an optional exact-or-subtree `path_prefix`, status filter, cursor, and limit, returns entries in stable ref order with a next cursor, and lists only members of the caller's owned graph. Default to loaded members, including running agents and loaded agents awaiting further input; callers can request running-only results, a specific lifecycle status, closed members, or all owned members explicitly. Apply filters before pagination. Excluding a closed member from default results does not release its task path. It neither resumes agents nor creates response subscriptions. Keep routine entries to ref, nickname, role, task path, and status; canonical UUID remains available in the result. Model/reasoning and full task/message payloads need not be repeated in this directory.

V2 already has `list_agents`; reuse that familiar name and applicable shared machinery without importing V2 task/mailbox lifecycle semantics or changing V2's existing availability in this pass. Its unfiltered listing covers the root control's live graph, not merely the caller's siblings or descendants; relative `path_prefix` values resolve against the caller's path. Keep the same unfiltered graph scope for V1, with the explicit status/pagination contract above.

V1 directory discovery requires explicit user enablement and supports explicit user disablement. When disabled, omit the model-facing tool and reject stale or previously discovered invocations at execution. `/agent sends` does not enable directory discovery, and enabling discovery grants neither messaging nor spawning permission. Skills and supervisor prose cannot enable it. Disabling discovery does not retract already seen identities or prevent permitted sends to known refs/UUIDs. Human `/agent` inspection and the app-server alias-list API remain available independently.

When enabled for the graph, directory discovery applies to its current and future members, including leaves that cannot spawn. Do not infer enablement from task-path use, model metadata, or role selection. Reuse existing user-controlled tool-availability configuration where possible; a live user override, if exposed, must follow the same admission gate and must not be restored merely from historical tool calls.

Permission updates need not enumerate all agents. Reuse existing identity/context machinery for attributed senders and authorized route hints, avoiding repeated UUID lists and repeated tool instructions. These limited hints are distinct from directory enumeration; do not inject the whole directory automatically as a substitute for disabled `list_agents`.

Preserve sender→recipient presentation in Main for peer communication, marked presentation-only relative to Main's model. Such display copies must not become Main model inputs or accidental subscriptions. Live, resumed, paginated, non-paginated, Review, and Full transcript projections must retain attribution and full auditable payloads under their existing presentation contracts.

## V1/V2 interoperability boundary

Share identity, attribution envelopes, and transcript projection where semantics match. Retain V1's single `send_input` plus `w` contract. Do not import V2's task/mailbox lifecycle merely to obtain readable paths or sender attribution.

A later V2 `send_message`/`followup_task` consolidation can build on this common representation. Actual cross-version routing still requires an explicit lifecycle/delivery mapping; common paths or envelopes alone do not prove transport interoperability.

## Implementation sequence

1. Add trusted attribution for V1 agent-originated input and concise permission-change context.
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
- Knowing a sender's ref/path does not grant reverse permission.
- Parent task dispatch remains available; directed and inherited permissions behave independently of commentary/final subscriptions.
- Permission updates do not wake idle agents, repeat unchanged state, or instruct agents to stop.
- Future children discover current peers/permissions without inheriting parent conversation history.
- Task targeting agrees across tools and TUI; ordinary duplicate-path spawns fail for both live and closed members, including concurrent spawns, without publishing a conflicting agent.
- Nested spawns and relative selectors/prefixes resolve against the caller's task path; absolute paths remain root-scoped, callers without a task path use `/root`, and unfiltered directory discovery is not restricted to siblings or descendants.
- Verify actual V1/V2 child and grandchild request payloads with task labels present and absent: role defaults, conflicting configured child defaults, explicit model/effort overrides, model-only reasoning reset, and role base/developer instructions must match the existing spawn contract. Include user TUI dispatch and close/unload/resume preservation of resolved settings.
- Cover default no-history spawns and explicitly authorized history forks with independent model/reasoning overrides; preserve service-tier and `w` behavior at nested depth without weakening existing admission checks.
- Separate roots can reuse paths; adoption allocates collision-free suffixed paths for the imported subtree atomically, reports the mapping, and preserves UUIDs and historical attribution.
- Directory filtering defaults to loaded members, supports running/status/closed/all selection before pagination, and does not release paths hidden by a filter.
- V1 `list_agents` is absent and rejects stale calls by default; explicit user enable/disable affects current/future graph members independently of sends permission, while human inspection remains available.
- Rename, close/resume, adoption, and history fork preserve canonical identity without restoring stale live grants or confusing separate roots with identical paths.
- Renames preserve send-time attribution while directory lookup uses current labels.
- Revocation after a `w:m` grant rejects a subsequent unauthorized send; queued messages recheck permission at target admission.
- Downward disable commands cannot block supervisor dispatch or leave misleading disabled-policy presentation.
- Non-paginated rollback and paginated revert preserve live permission state and reconcile model context, including runtime replacement, compaction boundaries, and inherited history. Fork-at-turn does not copy live grants, and removed work cannot leave stale queued input/delivery behind.
- Warm resume can inherit a surviving supervisor default without restoring ended directed grants.
- Compaction/rollback and both history modes preserve attribution and correct effective context.
- Main's peer presentation copies remain absent from Main model input.
- JSON/TypeScript/generated exports and Responses API item identities match the chosen typed representation; include request-payload integration tests and TUI snapshots.
