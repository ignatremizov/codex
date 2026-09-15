# Guardian circuit-breaker errors

Set `auto_review.circuit_break_action = "strict"` to include `TooManyDenials` in
`TurnAborted.error` when Guardian reaches its denial limit. App-server exposes it
as `turn.error.codexErrorInfo = "tooManyDenials"` in notifications and history.

The default, `"default"`, leaves this error unset. Both modes preserve the warning,
denial limit, and interrupted status; neither emits a separate `Error` event.

# Advisory wait countdowns

`item/started` and `item/commandExecution/terminalInteraction` include nullable
`deadlineAtMs` metadata, expressed as Unix milliseconds. It estimates the current
initial command wait, empty terminal poll, or agent wait—not the lifetime of a
process. Initial command estimates use the actual completion timeout when one
applies, otherwise the clamped initial yield window. Unrepresentable estimates
are null. Pauses and scheduling can extend the actual wait.

Empty terminal polls announce their estimate after acquiring and revalidating the
process's interaction lock, then clear it with null before releasing that lock on
ordinary success or failure. `itemId` remains the original exec item ID, not a
poll ID. Clients should match the turn, exec item, and process identity, clear
countdowns on turn finalization or status replacement, and never restore them
as live countdowns during history replay. Cancellation releases the lock without
terminating the process or sending a delayed clear. An individually cancelled
poll with no subsequent visible lifecycle event may retain its advisory estimate
until expiry; an interrupted turn clears it immediately.

# Human command approval deadlines

The optional `approval_timeout_ms` configuration sets a fail-closed deadline for human command approvals. When the deadline expires, the command is denied without execution. Approval request payloads expose nullable `startedAtMs` and `expiresAtMs` values; omitted timing remains untimed for compatibility with older clients. This policy applies only to human command approvals and does not change patch approvals, Guardian reviews, permission requests, hooks, or other elicitation types.

# Legacy command history reconstruction

Cold history reads can reconstruct `exec_command` and `write_stdin` results from
top-level raw tool records in Legacy rollouts. Poll output updates the original
command item even when the poll belongs to a later turn; rolling back that later
turn restores the command's earlier output. Persisted command lifecycle records
remain authoritative, including their output and attribution.

The first session header determines the reconstruction mode and initial working
directory. An existing header without `history_mode` defaults to Legacy.
Headerless raw records, and raw records preceding a delayed header, are not
backfilled. Embedded ancestor headers cannot change that decision. Paginated
history and compaction replacement context do not synthesize command items.
These reads do not restore live process ownership.

# Goal mutation and fork semantics

`thread/goal/set` preserves an existing goal's identity and accumulated usage when
editing its objective, status, or budget. An unchanged request is a no-op: it
does not restart continuation, rotate goal authority, or emit an update.
`thread/goal/clear` is also a no-op when no goal exists. Runtime effects are
applied before mutation responses are sent; obsolete effects cannot reactivate
a subsequently stopped or replaced goal.

Ordinary forks do not inherit goals. Explicit safety-retry forks preserve the
source goal's exact status, budget, and accounting, with automatic continuation
deferred until the first admitted turn. An active goal is not converted to paused.
Compaction reconstructs active goal guidance from the persisted objective.

`skills/list` may use the last valid configuration after a transient reload
failure only for the same working directory and unchanged, readable configuration
layers and managed requirements. The server logs the reload warning; requests for
other working directories still report their configuration errors.

# Model catalog provider requirements

`model/list` and periodic model catalog refreshes check the startup provider against
current managed provider requirements before using the catalog. If that provider no longer
complies, `model/list` returns JSON-RPC error `-32600` asking the client to restart Codex,
and background refreshes skip the old endpoint. Requirement load failures also block these
operations. Checks apply even when the catalog is cached. Existing startup provider selection
and caching behavior remain in effect while the provider satisfies current requirements.

# MCP App UI

`mcpToolCall.mcpAppUi` records the invoked descriptor's `resourceUri`
and explicit `preferredModelDisplayMode` (`inline` or `fullscreen`). Missing or
unsupported preferences leave `mcpAppUi` unset; `mcpAppResourceUri` retains the URI
so clients can apply resource display defaults. The
UI information is preserved in tool-call events and saved history so clients can
render without waiting for the full MCP catalog.

The field is null for older history and tools that declare widgets only in
result metadata; clients retain catalog discovery for those calls. Existing
resource URI fields remain available for older clients.

# Initial Daybreak choice (experimental)

Persistent threads accept `daybreakEnabled` on `thread/start` with the
`experimentalApi` opt-in. The response and `thread/started` notification both
include the initial choice in `thread.daybreakEnabled`. The choice is staged
with the thread's other initial metadata and saved when the thread is persisted.
An unused thread is not guaranteed to survive restart. Omitted or null leaves
the choice unset. Ephemeral threads cannot save it.
Use `thread/metadata/update` for later changes. This preference does not select
`turn/start.cyberAccessProgram` or grant access to an access program.

# Application network policy

App-server loads application network policy at startup and existing explicit
config/account reloads. Local requirements-file edits take effect on
the next explicit reload or restart. Installing a new policy cancels requests
that it no longer permits; a failed policy load blocks network traffic.

Embedded app-server installs the same policy-aware requirements loader for clients
it constructs. The embedding TUI and exec runtime install the same policy before
creating their telemetry providers, background HTTP clients, and executor
connections. TUI worktree cloud loaders retain that shared policy on reload.

# User verification cancellation (experimental)

Local UI clients can cancel a native user-verification RPC by sending
`userVerification/cancel` with `{requestId}` and the `experimentalApi` opt-in.
The result is an empty acknowledgment (`{}`). This API does not enable desktop
verification capability advertisement.

`requestId` is the original status, enroll, delete, or verify RPC's string or
integer ID on the same connection, not the server elicitation ID. Use fresh IDs
for each operation and a distinct ID for the cancel RPC. Unknown, finished,
unrelated, and other-connection requests are no-ops.

The acknowledgment confirms the cancellation signal without waiting for the OS
prompt to close. The original RPC completes independently, with
`cancelled/interrupted` when cancellation prevents completion. Cancellation
cannot roll back completed effects. It remains effective while a proof waits for
outbound queue capacity, but cannot retract a response already enqueued.

Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. Clients must cancel that RPC separately and discard
late proofs after the approval is canceled or resolved. Only one native worker
runs per app-server; if an OS call remains active after cancellation or timeout,
subsequent local operations return `failed/providerError` until that worker exits.

# Hosted Codex Apps MCP protocol

The host-owned HTTP `codex_apps` server uses Legacy by default in app-server and
standalone Codex. To discover the 2026-07-28 protocol, set
`codex_apps_mcp_2026_07_28 = true` under `[features]`, or send a true runtime
override via `experimentalFeature/enablement/set`. Discovery falls back to Legacy
when the server does not support it. Explicit config takes precedence.
The dedicated setting does not apply to third-party HTTP or local `codex_app`
stdio servers. The existing `mcp_2026_07_28` flag still governs eligible other
servers, regardless of whether their names or URLs resemble hosted Apps.
App-server does not persist this selection.

## Hosted resource reads

`mcpServer/resource/read` accepts `target: {connectorId, linkId}` for direct hosted app reads without tool discovery. A string `linkId` selects that account; `null` explicitly requests no-auth access subject to backend policy. Do not infer no-auth access from unknown or synthetic links.

`originCallId` with `threadId` takes precedence and retains the originating app/account scope. Requests without `target` retain discovery; `connectorId` continues to restrict reads to that connector. Direct targets require backend support for app/account resource reads.

# Project trust

`thread/start` does not persist project trust for a directory where configuration
discovery finds no project-root marker, Git checkout, or project-local `.codex`
directory. Starting a task there does not preapprove project configuration added
later. Existing trust decisions and permission checks for projects are unchanged.

# Thread removal

`thread/delete` deletes only the selected active or archived thread and emits one `thread/deleted` notification. Spawn, adoption, and communication relationships do not cascade deletion: related threads and their durable graph relationships are retained. Missing rollout files are treated as already deleted. This does not expand the authority to resume or transfer a surviving thread.

Deletion continues to enforce writer-ownership checks for the selected thread and is rejected when another paginated thread physically references its history. Archive retains its existing descendant handling and ownership checks.

`thread/archive` and `thread/delete` reject attempts to remove a live internal
worker with JSON-RPC error `-32600`. The worker's owner controls its shutdown.
For example, a Guardian reviewer remains available to its parent conversation
after a client tries to archive or delete it.

After the owner releases the worker, its saved conversation can be archived or
deleted normally. Ordinary client-controlled threads keep their existing behavior.

Thread deletion preserves shared message-board history even when the board feature is
disabled. A populated board blocks deletion because exclusive cleanup ownership cannot
be established. Missing or empty selected boards need no cleanup and are not tombstoned.
An unreadable or corrupt board database also blocks deletion before saved thread data
is removed; deletion never backs up and resets the shared store. Any recovery must be
performed separately before retrying, and populated-board checks still apply afterward.

## User verification (experimental)

Codex app-server advertises `openai/elicitation.userVerification` to the
host-owned plugin service for bundled, in-process TUI sessions (`codex-tui`) and
local stdio desktop sessions (`Codex Desktop`) on devices with supported biometric
hardware and the `experimentalApi` opt-in. This is an app-server decision,
independent of whether a key exists; TUI/Desktop/mobile do not advertise this MCP
capability. Mobile integration requires a separate rollout. Other clients and
network connections do not receive this mode, even with a recognized client name.
Before sending verification requests to desktop sessions, deploy a GUI that
handles the typed verification request, cancellation, and late proofs. The general
`experimentalApi` opt-in does not identify a compatible GUI version.

Native `openai/userVerification` elicitation requests preserve optional `_meta`
JSON through MCP transport and `mcpServer/elicitation/request`. Clients may use
this metadata for extension-specific presentation and must continue to accept
requests without it. Metadata does not change the challenge bytes or the proof
returned in the acceptance response.

Local UI clients use five methods. They require the existing
`experimentalApi` opt-in. The local provider reports
`unavailable/providerUnavailable` on unsupported platforms or without the required
ChatGPT account identity.

| Method | Params | Result |
| --- | --- | --- |
| `userVerification/status` | `{}` | `{credentialId, unavailableReason, unavailableMessage}` |
| `userVerification/enroll` | `{}` | `{credentialId, algorithm?, publicKey?}` |
| `userVerification/delete` | `{}` | `{}` |
| `userVerification/verify` | `{challenge, title, description}` | `{proof: {credentialId, signature}}` |
| `userVerification/cancel` | `{requestId}` | `{}` |

Status reads local readiness without prompting or contacting a backend. A null
`unavailableReason` means local checks passed, not that registration is valid.
Unsupported platforms and missing account identity are reported in the status
response's `unavailableReason` field.
Enrollment creates or reuses the local key and returns its public metadata. The
`publicKey` is unpadded base64url SPKI-DER; `algorithm` is `ecdsaP256Sha256X962`.
During the experimental rollout, `algorithm` and `publicKey` are optional for
compatibility with older app-servers. Current servers populate both fields;
callers must check that both are present and non-null before backend registration.
The trusted UI host owns backend registration: obtain an enrollment challenge,
sign it with `userVerification/verify`, check that the proof's `credentialId`
matches this response, and submit the public metadata and proof to the backend.
Local success is not server enrollment. The caller must preserve the authenticated
account across this flow and reconcile uncertain registration before retrying.
Deletion removes the local key; the caller owns backend revocation.
Enrollment and deletion coordinate credential lifecycle; callers do not issue
separate generate or rotate commands. Identity comes from the authenticated
account; this API exposes no caller-selected scope.

Verify signs 1–4096 decoded challenge bytes using P-256 ECDSA with SHA-256. The
challenge and DER signature use unpadded base64url. Title is 1–256 UTF-8 bytes;
description is at most 4096 bytes. The UI obtains approval for that display
context before calling. Verify does not require a pending elicitation; a UI with
its own authenticator can return proof directly in elicitation response content.
The calling flow owns pending-request checks and discards late proofs.
Native enroll, delete, and verify accept local stdio and in-process connections.
WebSocket and remote-control peers must use their own device authenticator;
status remains available for local readiness. Dropping an embedded RPC, disconnecting,
or changing authentication cancels its native operation. Responses recheck the
captured identity after waiting for outbound queue capacity.
Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. The GUI must use `userVerification/cancel` to
cancel that RPC and discard late proofs when an approval is canceled or resolved.
See [User verification cancellation](#user-verification-cancellation-experimental)
for request ID and acknowledgment semantics.
Only one native worker runs per app-server. If an OS call remains active after
cancellation or timeout, subsequent local operations return `failed/providerError`
until that worker exits.

Failures use the normal JSON-RPC error envelope with closed `{type, reason}` data:
`invalidRequest`, `unavailable`, `cancelled`, or `failed`. UI clients branch on
these values rather than message text. Native diagnostic payloads stay private.

## Sub-agent activity

`subAgentActivity` items include a nullable `prompt` containing the readable task text for started or interacted activities. Plaintext delivery exposes the original text; encrypted-with-audit delivery exposes the supplied audit copy, never ciphertext. Fully encrypted delivery and activities without readable task content use `null`. Live item notifications and persisted history preserve the full prompt; display limits are client-side only. Older history without the field remains readable.

## Inter-agent transcript items

Canonical `collabAgentToolCall` items include nullable `observeCommentary` and
`wakeOnCompletion` fields describing the V1 call's response-observation policy. Historical
items may omit them. These fields describe the source call; they do not authorize a client to
create or restore an observation. Core owns the exact target-turn subscription and accepted
delivery. Canonical policy survives subsequent lossy legacy mirrors, and a later canonical
item remains authoritative, including when it clears the metadata.

Incoming inter-agent communication is projected as typed `agentMessage` items in live `item/started` and `item/completed` notifications and saved history, independently of the experimental raw-response opt-in. `interAgentSource` is a nullable object containing `author` and `recipient`; ordinary assistant items use `null`, and older history may omit it. This is presentation provenance, not an authorization or delivery receipt. Clients must not treat these items as completion of the assistant's current answer or execute UI directives embedded in their text. Encrypted or mixed encrypted/plaintext payloads produce an opaque placeholder, including encrypted-with-audit delivery on the receiving side.

Canonical item and turn IDs are retained through live and historical projection. Public `thread/inject_items` agent messages receive host-owned IDs. Idle injection creates a completed history-only turn without starting inference; active injection belongs to the receiving turn. A running `thread/resume` response establishes the history boundary for that connection before subsequent live delivery, without suppressing other subscribers. A cancelled or failed resume releases suppression but requires another canonical resume to recover notifications suppressed during the unsuccessful snapshot; clients must not assume that delta stream is complete. Accepted publication waits for the canonical writer flush before live installation and event enqueue; this is not an fsync or client-observation guarantee. See [multi-agent message delivery](../../docs/multi-agent-message-delivery.md) for plaintext wrapper and audit behavior.

## Background child completion

Terminal child results can appear as canonical `agentMessage` commentary items without an active wait. They are child results, not the parent's final answer. The core-to-public conversion validates private completion provenance; context-only completion records are not duplicated into the public transcript. Private wait-ownership and completion metadata are not added to the app-server wire types.

`collabAgentToolCall` items include `receiverAgents`, an array of receiver metadata with `threadId`, nullable `agentNickname`, and nullable `agentRole`. Older records that omit the array decode as empty. A terminal wait remains a separate visible event from an earlier background row; clients must not deduplicate by matching result text.

Persistent completion delivery is bound to the exact parent runtime and follows the canonical writer-flush barrier. Ephemeral runtimes use explicitly runtime-only receipts and do not promise cold replay. Primary event enqueue is not a client-observation receipt, and an uncertain write does not permit an automatic retry or rebinding to a replacement runtime. See [background completion behavior](../../docs/tui-background-subagent-completion.md).

## Completed context compaction

Completed `contextCompaction` items include nullable `summary`, `message`, and
`decodeError` fields and an `availableSkills` string array. The summary is the compacted text when
available. For local compaction, the message is the complete compacted prompt;
for remote compaction, it is the display-only decoding of the installed handoff.
Decoded text never replaces the authoritative model history. Started items and
older saved history can have null text payloads; an omitted historical
`availableSkills` defaults to an empty list. The inventory describes the latest
model-visible skill names in the installed history, not newly activated skills.
Render the canonical `item/completed` item once; the deprecated `thread/compacted`
notification is not a second completion.

Remote handoff decoding sends a live-only `item/contextCompaction/status`
notification with `threadId`, `turnId`, `itemId`, and a `message` such as `Decoding`.
Apply it only to the matching active compaction; it does not start a new item,
reset its elapsed time, or appear in persisted replay. A decoder failure is
reported by `decodeError` on the completed item without undoing successful
compaction. Historical items lacking this field deserialize as null. Cancellation
and intentionally skipped decoding are not failures. Clients should show this
diagnostic even when compacted text is hidden.

Local compaction uses a 15-minute response deadline, an output limit of half the
model context window when known, and bounded session metadata appended to its
summary. These local safeguards do not change the remote V2 compaction service's
limits.

## Local rollout compression

The experimental `rollout/compress` method takes no parameters and immediately
returns `{}` after scheduling one best-effort background pass over the app-server's
local rollout storage. It does not change `features.local_thread_store_compression`
or require that startup flag to be enabled. Non-local thread stores do not support
this method.

The worker retains its existing cold-file checks, maintenance and writer locks,
concurrency limit, and cooldown. Acknowledgement does not imply completion or that
any files were compressed; failures are reported through existing logs and metrics.
There are no progress notifications or cancellation API. Clients sharing this
Codex home must support compressed rollout files, including shared histories.

## Managed model provider requirements

Existing threads retain their provider configuration. Input RPCs reject requests when managed
`model_provider` or `model_providers` requirements no longer match that configuration, or cannot
be loaded. This covers turn start/steer, review, compaction, manual queue start, and active goal
updates. Realtime connections use separate routing configuration and are not checked here.
Interrupt, realtime stop, and goal pause/clear remain available. User and project
configuration changes alone do not invalidate existing threads.

# Amazon Bedrock authentication

If `model_providers.amazon-bedrock.aws.credential_export` is configured, Bedrock setup and
Bedrock login return an error without changing configuration or saved credentials. Remove the
exporter configuration before selecting another credential source. `aws.credential_export` and
`aws.profile` cannot be configured together.

Application network restrictions apply to each AWS credential and region HTTP request and to
the Bedrock destination. Static access keys with an explicit region need no credential discovery.
AWS profile `credential_process` commands are run by the AWS SDK; their network traffic is outside
the application's HTTP policy. Configured credential exporters and AWS reauthentication commands
require unrestricted application policy; policy revocation cancels their active work.

## Live thread direct-input capability

`thread/read` projects loaded threads from their live runtime while retaining available
stored metadata. Start, read, resume, and fork responses for loaded threads report
`canAcceptDirectInput: true`, including spawned children. Unloaded stored threads report
`null` because their live capability is unavailable. This field does not bypass ordinary
turn validation, active-turn requirements for steering, or managed provider requirements.
Loaded spawned-child list and search results expose the same capability. Parent ownership alone does
not prohibit direct turns, steering, settings updates, or other thread input. Queueing
input for an unloaded spawned child still requires resuming that child first.

Persisted V2 children can be resumed through their live owning control only when the direct parent is loaded and its ownership identity matches the recorded spawn edge. An absent or mismatched owner fails recoverably; resume does not create a detached fallback control. Generic `thread/resume` continues to accept stored child IDs and paths, while caller configuration overrides remain subject to the ordinary config and permission rules. Loaded sessions with subscribers or active work are not replaced; an idle session without subscribers can be replaced only after shutdown completes. Parent-driven follow-up tasks retain their separate inherited-instruction and role configuration behavior. See [agent restoration lifecycle](../../docs/agent-restoration.md) for the direct-input, TUI, and legacy V1 boundaries.

For an owned V2 cold resume, an explicit `developerInstructions` or
`config.developer_instructions` overrides the role's developer instructions.
An empty string is an explicit override; omission reapplies the configured role.
This does not override other role restrictions or the child's recorded model routing.

## User-controlled agent dispatch

The experimental `agentAlias/list` method lists durable aliases in a root-scoped namespace, including canonical thread IDs, refs, nicknames, and lifecycle state. The experimental `agent/control` method authorizes user-authored spawn, prompt, reserved/queued prompt, resume, interrupt, close, and observation operations from a source thread. Responses identify the canonical target and admitted submission where applicable; post-admission or audit persistence warnings do not turn committed work into retryable failures. Omitted response handling is passive, and close response handling replays according to the supplied policy. Input outcomes distinguish `queued`, `admitted`, and `unknown` work; unknown inputs must be reconciled from canonical history before retrying.

The `spawn` action accepts optional `model` and `reasoningEffort` overrides. Explicit values take precedence over role settings and configured subagent defaults; omitted values follow the normal child configuration resolution. The durable `userAgentControl` item records the requested model and reasoning values separately from the canonical target and outcome. Existing history-fork and lifecycle authorization rules still apply.

Agent-originated input is projected as `agentMessage` with optional `attribution` and structured `input`. Attribution captures sender and recipient identity snapshots, including canonical thread IDs, refs, nicknames, task paths, role, model, reasoning effort, and the sender turn ID. These fields are presentation metadata and do not authorize routing; older messages without them retain their existing rendering. Human-authored spawn, prompt, and queued input remains `userMessage` in the target thread.

`spawn` accepts an optional `task` label, and successful spawn/resume outcomes expose the committed `taskPath`. Resume adoption may also return `taskPathMapping`; these labels are nullable assignment metadata and never replace canonical UUID/ref identity or lifecycle edges.

Final-response observation is directional too. The user command `/agent observe <target> [from <observer>] <passive|wake|presentation>` maps to `agent/control` with an optional `observer` selector:

```json
{
  "sourceThreadId": "<main-thread-uuid>",
  "action": {
    "type": "observe",
    "target": "Main",
    "observer": "2",
    "authoredObserverSelector": "ref:2",
    "responseHandling": "presentation"
  }
}
```

This changes agent 2's existing observation of Main, not Main's observation of agent 2. Omitting `observer` preserves the issuing thread as observer. Both endpoints resolve within the issuer's controlled graph and must have current live runtimes. Observation replaces only an existing active, pending, or undelivered final-response subscription; it does not create a subscription, grant messaging permission, resume an agent, or start a model turn. `sourceThreadId` remains the authorization and audit issuer even when another observer is selected. The `observed` outcome returns the resolved `observerThreadId` and `targetThreadId`. The issuer's durable `userAgentControl` item records nullable `observerThreadId` and `authoredObserverSelector`, including the authored observer on rejected requests. Clients may provide `action.authoredObserverSelector` to preserve the original `ref:` token or quoted nickname separately from normalized `action.observer`. This raw token is audit-only: routing and authorization use `observer`, never the authored token. When an explicit observer is present but the authored field is omitted, the audit falls back to `observer`. When `observer` is omitted, the authored field is ignored and the audit's authored observer remains null. Historical items without these fields deserialize as null. Clients must not substitute the issuer for an unresolved explicit observer when displaying an error or updating directional state. If selected-observer publication loses its canonical receipt, that observer requires reload and the issuer audit records an `unknown` outcome, not a known rejection; do not automatically retry the change.

The `replyRoute` action enables or disables a V1 target's attributed replies to the source across later turns of that live runtime. Enabling adds route guidance to the target's model context once; disabling rejects new replies even when a model-authored send uses `m`. Already accepted human prompts keep their queued input and captured response policy. Unsupported V2 targets reject `replyRoute` before mutation. Saved context alone does not restore live reply authority after a cold resume or fork. An indeterminate route update is audited as `unknown` and must be reconciled rather than retried.

Reply routes may name a recipient explicitly; omitting it addresses the source thread. A `subtreeMessaging` action can set a live default for a source and its descendants. Explicit reply-route settings override that default, and neither permission is restored by replaying history or by a cold resume.

The experimental `agentQueue/list` and `agentQueue/delete` methods expose and cancel pending target-owned FIFO entries. Queue acceptance is not target-turn admission: queued entries have no synthetic turn ID, and their start metadata records the source and response policy once a target turn actually begins.

`thread/mailbox/add` accepts `{threadId, input, clientUserMessageId}` as user-authored mailbox input for explicit receiver consumption. It returns a stable `messageId` and `pending`, `claimed`, `consumed`, or `rejected` state, never a turn ID. Retrying the same receiver/client message identity returns its stored state and preserves typed input and attachments; mailbox acceptance does not load, adopt, or start a payload-bearing turn. Unloaded receivers retain pending mail until a compatible runtime is loaded, while fresh deposits to a known unsupported backend are rejected before persistence.

`thread/mailbox/read` returns a payload-free aggregate snapshot with `pendingTotal` and canonical `pendingSenders`. It does not load, claim, or consume messages; storage/backend unavailability is distinct from an empty mailbox.

Collab-agent history may expose nullable `agentRef`, `taskPath`, and `mailboxInput` presentation metadata. `agentRef` is the trusted root-scoped numeric alias encoded as a string; task paths are assignment labels, not lifecycle ancestry. Clients must not infer missing refs from task paths, nicknames, prompts, or tool-output text, nor infer mailbox delivery, receiver consumption, or visibility from missing legacy fields.

Direct `check_mail` completion is represented by a payload-free `mailboxRead` item containing the selector and acknowledged consumed/rejected counts. It is emitted only after the fixed batch acknowledgement and never includes message bodies; mailbox consumption from `wait_agent` does not emit this item.

## User shell commands

`thread/shellCommand` runs a user-authored command with full access, independently of the thread's model-turn lifecycle. Its immediate acknowledgement does not mean the process has exited. Omitted or null `timeoutMs` uses `user_shell_command_timeout_ms`, which defaults to no deadline; a positive value sets a deadline and an explicit request value of `0` requests an immediate timeout.

Optional `responseHandling` selects `finalDelivery` (`passive`, `wake`, or `presentationOnly`) and `queueCommand`. Omission keeps passive, concurrent execution. With `queueCommand: true`, execution waits for earlier user-shell submissions in the same thread; later requests without queuing can still run concurrently. This execution queue is separate from agent input queues.

Commands emit `item/started`, optional `item/commandExecution/outputDelta` notifications, and `item/completed` with the same `commandExecution` item ID and `source: "userShell"`. An idle-thread command uses a standalone activity ID; later model turns can start while it runs. An active-thread command uses that turn's presentation identity without inheriting its cancellation. Command items expose the resolved policy as `userShellResponseHandling`.

`item/commandExecution/terminalInteraction` may include a `wait` lifecycle object for `write_stdin`. Started waits identify the interaction, start time, and mode (`timed` or `untilExit`); finished waits include elapsed time and a reason such as `exited`, `timeout`, `input`, `cancelled`, or `failed`. Older events may omit this metadata.

Sleep items may include optional `outcome` and monotonic `elapsedMs` completion metadata; legacy history may omit both fields.

Passive and wake results are canonically recorded and installed in model context before any continuation is queued. A continuable active turn receives a scheduling signal, not another copy of the output. Otherwise passive delivery does not start a turn, while wake delivery requests a model turn through ordinary admission after publishing the completed activity. Interrupting or shutting down before that continuation runs cannot discard an acknowledged result. Presentation-only results remain visible in thread history without entering model context or starting a turn. Stopping a wake-enabled command downgrades its result delivery to passive.

```json
{ "method": "thread/shellCommand", "id": 26, "params": { "threadId": "thr_b", "command": "git status --short", "responseHandling": { "finalDelivery": "wake", "queueCommand": true } } }
```

`thread/backgroundTerminals/list` includes the command's process ID and `userShellResponseHandling`. Use `thread/backgroundTerminals/terminate` to stop one process or `thread/backgroundTerminals/clean` to stop all; a model-turn interrupt does not stop these commands. Thread shutdown requests cancellation, but a cancellation request alone is not confirmation of process exit or durable output teardown.

Shutdown drains accepted shell output producers before closing canonical history publication. An unrelated model turn completing or failing neither stops the shell nor turns its live output into a failed model-tool row. Live rendering retains a matching admitted command without pretending that the model is busy.

## Stored thread attachments

- `thread/attachment/add` — add a durable resource reference to a stored thread without loading it. Repeated writes with the same attachment type and identity key return the existing attachment.
- `thread/attachment/list` — list attachments for one stored thread in a cursor-paginated request, including a thread that is not loaded.
- `thread/attachment/remove` — remove an attachment by its thread, attachment type, and identity key; returns `{}`.
- `thread/attachment/updated` — notification broadcast after an attachment is created or removed; contains the thread, attachment identity, attachment id, and operation.
### Example: Manage stored thread attachments

Attachments record the resources currently associated with a thread, independently of conversation history. Clients can add, remove, and list attachments for one stored thread at a time without resuming those threads. Adding or removing an attachment does not create or delete the underlying resource or rewrite history. An attachment is idempotently identified by its thread, `attachmentType`, and `identityKey`. For pull requests, clients should reuse the canonical application identity `JSON.stringify([canonicalHostname, lowercaseOwner, lowercaseRepository, pullRequestNumber])` so addition and removal agree across surfaces.

```json
{ "method": "thread/attachment/add", "id": 20, "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
    "payload": { "url": "https://github.com/openai/codex/pull/123" }
} }
{ "id": 20, "result": {
    "outcome": "created",
    "attachment": {
        "id": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
        "attachmentType": "pull_request",
        "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
        "payload": { "url": "https://github.com/openai/codex/pull/123" },
        "createdAt": 1750000000
    }
} }

{ "method": "thread/attachment/list", "id": 21, "params": {
    "threadId": "thr_123",
    "limit": 100
} }
{ "id": 21, "result": {
    "data": [{
        "id": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
        "attachmentType": "pull_request",
        "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
        "payload": { "url": "https://github.com/openai/codex/pull/123" },
        "createdAt": 1750000000
    }],
    "nextCursor": null
} }

{ "method": "thread/attachment/remove", "id": 22, "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]"
} }
{ "id": 22, "result": {} }

{ "method": "thread/attachment/updated", "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
    "attachmentId": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
    "operation": "deleted"
} }
```

`thread/attachment/list` accepts one `threadId` and returns at most 100 attachments per page, ordered by creation time and attachment id. Continue with `nextCursor` and the same `threadId` until the cursor is `null`. Each thread can retain up to 100 attachments. Removing an attachment frees a slot for a new attachment.

A non-ephemeral fork copies the source thread's current attachments, even when forking at an earlier turn. The copies have new attachment IDs and creation timestamps, but retain the same resource identities and payloads. Clients use `forkedFromId` on `thread/started` to detect forks and call `thread/attachment/list` with the new thread ID to load their attachments. Fork copying does not emit per-attachment updates; explicit add/remove operations still do. Copying is awaited before publishing the fork, but is best effort: a copy failure is logged and the conversation fork succeeds without attachments. Membership can then change independently on either thread; the referenced resources themselves are not copied. Resuming a fork does not repeat the copy.

Attachment creation and deletion requests using the same thread ID are serialized across connections. The requesting client receives its response before the compact update is broadcast, and duplicate creates or absent deletes do not emit updates. Deleting the owning thread removes its attachments under the same lifecycle exclusion; queued attachment mutations then report that the thread was not found.

# Thread plugin settings

`thread/settings/update` and `turn/start` accept `disabledPluginIds`, a list of
`PluginSummary.id` values from `plugin/list`, in the
`<plugin-name>@<marketplace-name>` format. A supplied list replaces the selection;
omission or `null` preserves it, and `[]` clears it. Saving this selection does
not yet filter plugin capabilities.

Read the selection from `threadSettings.disabledPluginIds` in
`thread/settings/updated` notifications, or from `disabledPluginIds` in
`thread/start`, `thread/resume`, and `thread/fork` responses. Selections persist
across resume. Forks restore the selection from the history retained at the
requested fork boundary.

# Cross-home paginated forks

Experimental clients may identify a local rollout with `path`. Paginated paths outside the active Codex home are copied with their inherited lineage into one standalone destination rollout, so the fork never depends on source-home files after creation. Source files are read-only; paths managed by the active store retain coordinated reference-backed fork behavior.

External source UUIDs are audit identity, not authority to inherit a same-ID local runtime's instruction provider, runtime settings, or attachment membership. Explicit destination providers and settings still apply. `deferGoalContinuation` requires a source in the active home; external history does not authorize an active-home goal lookup. Copying preserves original source evidence and rejects a lineage whose flattening would manufacture an observation or task-promotion proof across removed metadata, rollback markers, or source-file boundaries. It never repairs or rewrites the source to make that copy succeed.

# Deprecated thread personality setting

`thread/start`, `thread/resume`, `thread/settings/update`, and `turn/start` still
accept `personality`, but `friendly` and `pragmatic` no longer select a style.
`model/list` returns `supportsPersonality: false` for every model.

`none` removes the literal `# Personality` section when Codex prepares
instructions from the model catalog, for example when starting a thread or
switching models. Setting `friendly` or `pragmatic` can replace a previous
`none` setting for that purpose. Changing the setting does not rewrite the
thread's existing instructions or change explicitly supplied base instructions.
The old `features.personality` flag is ignored.

`thread/start` accepts `baseInstructions` and `developerInstructions` as resolved, thread-scoped text. Supplied base instructions have custom provenance; developer instructions remain separate from the base and managed guidance. The TUI forwards its configured custom instruction content when creating a thread on a shared server, including content loaded from a local profile's `model_instructions_file`, rather than asking the server to reopen the client's file. Starting one configured thread does not change the daemon defaults or another thread. Catalog-derived base text is not forwarded as a custom override.

# MCP server capabilities

`mcpServerStatus/list` returns `serverCapabilities` for each initialized MCP server
in both `full` and `toolsAndAuthOnly` detail modes, including thread-scoped reads.
This is the server's advertised MCP capabilities object, including its `extensions`
map. It is null when the connection has not initialized successfully; capabilities
are never inferred from tools or copied from a shared catalog cache.

Pass `serverName` to discover only that server. With `threadId`, the request
reuses the thread's current MCP connection and tool catalog after any pending
runtime refresh; discovery then waits only for that server. Without `threadId`,
discovery creates a connection for the selected server. An unknown name returns an empty page.
Omitting `serverName` preserves full-inventory discovery.

`allowImplicitInvocation` reports each server's effective prompt-exposure policy. A value of `false` does not disable the server, bypass approval rules, or remove authorized deferred discovery and calls.

`thread/mcpServer/activate` accepts `{ "threadId": "...", "serverName": "..." }` for a loaded thread and requests forward-only insertion of that server's full current tool inventory into model context. It does not start inference, rewrite the initial prompt, or promote the server into the frozen direct tool contract. Unknown or disabled servers are rejected using the thread's effective catalog and environment selection.

The response's `outcome` is `activated` when the operation is accepted, `alreadyActivated` when explicit-use context already exists, or `alreadyImplicitlyAvailable` when the current inventory is already covered directly. `activated` acknowledges admission, not completed startup or durable context insertion. Inventory capture runs at the appropriate actor boundary, and an unavailable inventory can be represented by an empty array. Clients must not interpret any of these outcomes as new tool-call authorization.

# MCP OAuth login

`mcpServer/oauth/login` only returns HTTP(S) authorization URLs. Authorization
endpoints with other schemes fail before client registration or URL return.

# Thread rollback

`thread/rollback` is retained as a Legacy-history compatibility route. Paginated threads continue to use `thread/revert`; rollback rejects them rather than changing their history mode. Neither operation undoes filesystem changes.

The request contains `threadId` and `numTurns` (at least one). For guarded prompt editing, send both `expectedStartTurnId` and `expectedTurnCount`: `numTurns` then selects a materialized suffix, whose first turn and total observed turn count are revalidated against canonical history. Without guards, the route retains its historical user-instruction-count behavior. Running turns, concurrent history mutations, and busy submission admission reject rollback without accepting the mutation. A successful response contains the updated `thread` with canonical turns populated and its current live status, rather than the stored summary's `notLoaded` status.

The Legacy mutation appends a rollback marker using a single-attempt recorder command, followed by any required reconstruction repairs, before installing the live history. A failed marker or required-repair barrier is not proof that nothing was written. The session remains quarantined until its writer is closed and canonical history can be reloaded; timeout does not authorize discarding a writer.

Error responses distinguish recovery from retry:

- An ordinary invalid-request error means the request was rejected.
- `error.data.threadRollbackCommitted: true` means the history mutation committed but response hydration failed. Reload history; do not repeat the mutation.
- `error.data.threadRollbackRefreshRequired: true` means the outcome or live state is unsafe to continue. Reopen through canonical recovery; do not repeat the mutation or submit further work to the old runtime.

A transport failure can also leave the mutation outcome unknown. The Rust client transports expose an internal, request-ID-correlated completion event to order rollback replies after preceding notifications in their event queues. This is not a new wire notification and does not certify commit or storage durability. Consumers must separately apply the response's outcome and replace stale transcript buffers with canonical history.

Historical count-only markers remain readable. New exact markers use positions in the full canonical decoded rollout, before filtering, compaction projection, or fork reindexing. Replay and migration preserve the surviving history, including terminal evidence for retained turns. Recorder acknowledgement means the existing file-flush contract, not an added filesystem `fsync` guarantee.

# Selected workspace routing

The experimental `account/read.workspaceRouting` response field returns the selected ChatGPT workspace's `chatgptAccountId`, resolved HTTPS `backendOrigin`, and backend-provided `accountRoutingOverride`. The routing value is `us`, `us_cr`, or the explicit `NO_CONSTRAINT` value. API-only and signed-out accounts return `null` and do not need `accounts/check`.

App-server discovers routing for saved ChatGPT logins at startup and for new logins or workspace switches. After requirements and routing are ready, it sends the existing `account/updated` notification. Newly initialized connections also receive this notification once saved-workspace routing is ready, including when discovery finished before the connection initialized. Clients then reread `configRequirements/read` and `account/read`. Saved ChatGPT credentials without a selected workspace ID retain their account information and return `workspaceRouting: null`; app-server does not guess a workspace from the backend's default account. Discovery failures for a selected workspace, including missing or null fields from older backends, return an `account/read` error. They never produce a successful unrestricted result. A later read retries failed discovery. Logout clears the cached routing, and results from earlier authentication owners are discarded. Token refreshes for the same known user and workspace invalidate cached routing without cancelling discovery or failing sign-in. Configuration is reloaded after discovery; a changed backend, model provider, or required backend rejects the result so the next read discovers against current configuration. Account notifications recheck the auth owner generation after waiting for outbound queue capacity. Superseded sign-in attempts emit a failed `account/login/completed` event instead of silently dropping completion. Notifications remain snapshots: clients reread current account and requirements state rather than treating a queued notification as authorization.

Routing compares origins by scheme, host, and effective port, ignoring API paths. A required
`chatgpt_base_url` must match discovery; if neither provides an origin, `NO_CONSTRAINT` uses the
configured base URL.

Responses HTTP (including compaction) and WebSockets wait for discovery and preserve API paths.
Guardian v2 classifier HTTP and pooled WebSockets use the same routing.
HTTP redirects are rejected. `us` and `us_cr` set `X-OpenAI-Account-Routing-Override`;
`NO_CONSTRAINT` omits it.

API-key and explicitly external-auth providers bypass discovery. Custom ChatGPT-auth destinations
require discovery before being treated as independent. Changing a workspace-bound thread's
bootstrap origin requires a new thread.

## Windows sandbox implementation selection

`windowsSandbox/setupStart` applies only to the legacy `elevated` and
`unelevated` backends. `windowsSandbox/readiness` reports `ready` when MXC is
selected so clients do not offer legacy setup. The
`allowedWindowsSandboxImplementations` requirement governs only the legacy
backends and does not restrict MXC. Its `mxc` enum member is retained for wire
compatibility but is not emitted. Non-Windows hosts report `notConfigured`.

MXC uses the standard `command/exec` streaming and process-control path, including
ConPTY when `tty` is enabled. The buffered legacy Windows sandbox restrictions on
process control and custom output caps do not apply to MXC.

### Gateway OAuth sign-in

Providers configured with `gateway_oauth` require a secondary OAuth credential in
addition to their primary authentication. Clients with a gateway sign-in UI set
`initialize.capabilities.explicitGatewayOauth: true`, complete initialization, and
successfully call `account/gatewayOAuth/read` before sending authenticated requests,
including startup `model/list` and inference requests. Repeat this probe on each
new connection. A successful `initialize` alone does not confirm support: older
servers can ignore the unknown capability and retain automatic browser login.

Support for `account/gatewayOAuth/read` and `explicitGatewayOauth` is introduced
together, so a successful read confirms support even when `required` is `false`
or `status` is `notReady`. The returned status determines whether sign-in is needed;
it is separate from the capability check. If the probe fails because the method is
unsupported, require a server upgrade. Other errors and timeouts also leave
authenticated requests blocked until a probe succeeds; do not silently fall back
to automatic login.

With explicit login enabled, app-server refreshes existing credentials, but only the
`account/gatewayOAuth/login` RPC starts browser authorization. Requests needing
sign-in fail promptly so the client can offer that flow.

Clients that omit the capability or set it to `false`, including the TUI, retain
automatic browser authorization after initialization. Startup credential reads
cannot open a browser before initialization. Explicit opt-in is shared by gateway
managers using the same home and network configuration within the process and
cannot be undone by a later connection that omits the capability.

- `account/gatewayOAuth/read` returns the current effective `providerId`,
  `providerName`, `required`, `status`, and `error`. `required` indicates that this
  provider uses gateway OAuth, including when already signed in. This operation
  does not refresh tokens or open a browser; `status` is null for other providers.
  `notReady` means credentials are not ready. `succeeded` means saved credentials
  are locally usable, not that a gateway request has been verified. Reads observe
  usable replacement credentials saved by another process sharing the same home.
- `account/gatewayOAuth/login` starts authorization and returns `{}` after
  the credential has been saved. Providers requiring OpenAI authentication need a
  primary account first. A second login request fails while a login is active.
  The initiating connection receives a `started` notification with `authUrl`; the
  client must open that URL in a browser that can reach the server callback port.
  Other status notifications set `authUrl` to null. Login is rejected if the
  initiating connection opted out of `account/gatewayOAuth/changed` notifications.
- `account/gatewayOAuth/cancel` cancels the calling connection's login and returns
  `{}` after the active login releases its slot, so the client can immediately
  start another login. Closing that connection also cancels its login and releases
  the callback listener. Cancellation makes the pending login request fail.
- `account/gatewayOAuth/changed` reports `notReady`, `started`, `succeeded`, or
  `failed`, with an optional `error`. Notifications apply to the current effective
  gateway configuration. Clients can read readiness when connecting and after
  changing configuration. Notifications follow the standard per-connection
  `optOutNotificationMethods` setting.
  These payloads never contain credentials.

Read, login, and cancel take no params. Read and login use the app's current
provider, reloading configuration and returning an error if it cannot be loaded.
Status notifications include `providerId`. During browser sign-in, inference
requests fail promptly and can be retried when sign-in succeeds.

`model/list` also checks gateway authentication before returning cached models.
If authentication fails after the provider configuration changes, it asks the client
to restart Codex so the retained catalog and gateway sign-in use the same provider.

## Application network policy

Application policy uses the same managed TOML merge as agent-network requirements:
higher-priority layers override conflicting values, including `enabled` and each
domain permission, while non-conflicting domain entries are retained. Omitted
values inherit from lower layers. After merging, a present network block defaults
to `enabled = true` and an empty domain map, meaning no external destinations are
allowed. An effective `enabled = false` disables application destination policy.
Domain keys are exact ASCII names, normalized to lowercase without a trailing dot
before merging; wildcards, URLs, ports, invalid permissions, and duplicate
normalized names are rejected.
App-server enforces these rules for its HTTP and WebSocket traffic before route
resolution or connection work, including redirects and reused clients. An allow
entry permits only HTTPS or WSS to that exact host. Agent-network requirements
remain separate in `network`.

App-server reloads effective requirements on explicit config or account reloads.
Local changes or read failures discovered on reload revoke active requests;
unchanged requirements preserve them. Failed policy loads block traffic until
requirements load successfully. Invalid request or project configuration does not
revoke unrelated traffic. Account changes revoke
outstanding requests and clients retaining the previous account's authorization.
Policy updates also stop active requests to newly denied destinations. Narrow
authentication and requirements-discovery clients use local requirements and
exact endpoint URLs while workspace policy is loading. API-key-only deployments
do not discover ChatGPT workspace requirements.

SDK transports without destination enforcement, including OTLP exporters and AWS
credential discovery/signing, are disabled while restrictions apply. Supported
HTTP, WebSocket, and code-mode gRPC requests use the shared destination checks.
User-directed Git, SSH, shell, and other subprocess traffic retain their existing
execution and sandbox policies.

# Item history anchors

`thread/items/list` accepts an optional nullable `cursor`: either an opaque string
from a previous response or an item anchor such as
`{"type":"item","itemId":"item-123"}`. An item anchor resumes exclusively after
that item in the requested pagination order: ascending (the default) returns newer
items, and descending returns older items. It requires a non-empty `turnId`;
otherwise the request returns invalid params (`-32602`) with
`turnId is required when cursor is an item anchor`.
The item must belong to that turn in the thread's visible history. Empty, unknown,
and out-of-scope item IDs return invalid params (`-32602`) with
`cursor.itemId does not identify an item in the requested history scope`.
Omitted or null cursors preserve normal first-page behavior. Continue anchored
pages with the returned opaque string `nextCursor`; response fields and
`backwardsCursor` semantics are unchanged.
