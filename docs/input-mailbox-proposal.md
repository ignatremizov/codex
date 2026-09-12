# Receiver-selected input mailbox

Status: implementation in progress; not yet executable-validated or released. This records the agreed mailbox workflow alongside the existing [V1 response-observation contract](multi-agent-v1-response-observation.md) and [agent attribution contract](multi-agent-v1-attribution-and-task-paths.md).

## Purpose

A supervisor can finish delegating before consuming early explorer results, then review those results in its chosen order. Users can likewise leave information without steering current work or scheduling a complete follow-up prompt. Mailbox acceptance, notification, consumption, and task admission are separate operations.

## Input modes

| Mode | Behavior |
| --- | --- |
| Steer | Deliver at the next supported input boundary during current work, following existing admission rules. |
| Queue | Admit a separate turn after current work finishes; preserve existing FIFO and response-delivery semantics. |
| Mailbox | Retain the payload until the receiver explicitly consumes it through `check_mail`. Notify with an inventory at an idle boundary, not with the payload. |

Agent submission uses `send_input(target, message, w:"z")`. Several submissions may accumulate without steering the receiver or immediately starting a payload-bearing turn. The sender receives acceptance, not an assertion that the receiver has read or acted on the message.

User input uses one-shot `/mail <message>` for the selected thread, alongside existing steer and queue modes. Attachment-only submissions are supported; no sticky mode or default key binding is added. The typed `thread/mailbox/add` endpoint returns acceptance state, not a turn. Failures preserve the rich draft, and unchanged retries reuse their submission identity. There is no fallback to steer or queue. User mail retains user authorship, attachments, and ordinary user transcript styling. Agent mail retains trusted agent attribution. Shared transport must not collapse their authority distinction.

## Consumption

Use a dedicated `check_mail` tool for explicit consumption:

```json
{"from":"2"}
```

```json
{"from":"/root/sepa/settlement"}
```

```json
{"from":"user"}
```

```json
{}
```

`from` selects one sender using existing agent selectors, or the reserved user selector. Omission consumes all pending mail. Matching messages are delivered in acceptance order; unselected messages stay pending. Sender selection resolves to canonical identity rather than trusting payload headers or mutable display names.

The tool description must make clear that checking delivers and consumes messages, not merely lists them. Consumption captures a fixed batch; messages arriving after that boundary remain pending. Preserve original structured input and trusted authorship in model delivery and audit history rather than flattening user content into agent-authored tool prose.

Consumption and durable delivery acknowledgement must prevent loss or duplicate model injection across retries and resume. Reuse existing input/admission/persistence facilities where appropriate instead of introducing another history authority.

Selection alone does not acknowledge consumption. Tie consumption acknowledgement to durable receiver-history delivery using the existing delivery identity machinery. Retrying the same invocation must recover its selected batch rather than select newly arrived mail. Specify recovery before and after that commit boundary during implementation.

Deliver payloads once through the appropriate structured user/agent input representation, preserving tool-call/result ordering. The tool result reports delivery metadata/status rather than duplicating payloads already inserted as structured input. In particular, user mail must not exist solely as quoted JSON inside a tool result.

Canonical `check_mail` results and tool hooks record acceptance as `{"status":"delivery_requested","from":...}`, with a canonical sender UUID, `"user"`, or `null` for all senders. Disposable model requests project a genuine call/result pair against its existing fixed claim. Terminal claims report `empty`, `delivered`, or `rejected`, with `delivered_count` and `rejected_count`. Counts cover the entire fixed invocation, including deliveries completed by an earlier retry; they do not count later arrivals. A mixed consumed/rejected batch reports `delivered` with both counts, and a nonempty entirely rejected batch reports `rejected`, not `empty`.

Model-visible `from` uses the receiving request's advertised short ref when available, otherwise the canonical UUID; `"user"` and `null` remain unchanged. Projection never creates a claim, grants authority, acknowledges consumption, or rewrites canonical history. Missing claims and nonterminal claims remain `delivery_requested`; unknown, error, and hook-rewritten result shapes are preserved. Real claim-read errors stop request preparation. The result stays before structured payloads. Normal sampling, local compaction, and remote-V2 disposable requests use this projection; legacy remote compaction keeps canonical results because it directly retains provider-returned history. `wait_agent` results are unaffected.

`check_mail` never consumes ordinary queued prompts or intercepts steer input.

### Targeted consumption through `wait_agent`

`wait_agent(B)` also consumes pending mail authored by B and addressed to the caller. It does not read B's inbox or consume mail from unrelated agents or the user. Multiple wait targets select the union of those senders, retaining acceptance order within the selected batch.

Return immediately when selected mail is already pending. Otherwise wait until selected mail arrives, an existing execution/completion condition is satisfied, or the timeout expires. Preserve existing wait status and completion semantics; mail arrival is an additional return reason, not evidence that an agent completed.

Keep execution status and mailbox delivery distinct: the tool result contains status and return metadata, while selected payloads follow it as structured input, not duplicate tool-result text. Reuse the same fixed-batch consumption and durable acknowledgement path as `check_mail(from:B)`, so concurrent or retried calls cannot inject the same mail twice. A wait that returns because of completion or timeout also captures any selected mail pending at its consumption boundary; later arrivals remain pending.

Direct calls add `return_reason` (`mail`, `completion`, `timeout`, or `recovery`) and, where needed, `mail_only_targets`. Existing fixed claims, including empty claims, return immediately for recovery without waiting for new arrivals or changing their selection. Nested/code-mode calls retain existing completion-wait behavior and do not claim or drain mailbox input. See the [targeted-wait contract](multi-agent-v1-targeted-wait.md).

Receiving mailbox content through a wait does not create a commentary/final subscription or consume unrelated queued/steer input. Existing completion-delivery arbitration must continue to prevent duplicate final responses independently of mailbox consumption.

Resolve mailbox sender selectors independently of lifecycle-controlled target resolution. Legitimately pending foreign-root mail can be selected without adoption; do not relax a shared lifecycle resolver to achieve this. Revalidate the applicable mail permission before consumption. Access to foreign execution status or final-response observation requires separately authorized observation, not merely a mail grant; finalize how a mail-only target is represented in the wait result without leaking unrelated output.

## Inventory wake

When newly pending mail exists, notify the receiver after its current turn ends, or when it is already idle. The notification starts a turn containing only a compact inventory, allowing the model to select which sender to consume first:

```text
Pending mail snapshot (not consumed):
{"receiver":"1","pending_senders":[{"from":"2","count":3},{"from":"4","count":1},{"from":"user","count":1}]}
check_mail: {"from":"<ref>"} or {} for all.
```

Payloads remain outside model context until selected. This supersedes the earlier suggestion that an ordinary non-`z` send automatically drains older mail: normal sends must not bypass receiver selection.

Model requests use only short refs advertised by that request's captured receiver-scoped identity mapping, including its descriptive nicknames. Foreign agents and identities omitted from that mapping use their canonical UUID instead; stored sender refs or nicknames are not selector authority. `receiver` follows the same ref-or-UUID rule, and user mail uses `"from":"user"`. Counts describe the frozen inventory snapshot, not a guarantee of the next `check_mail` result: later arrivals can change what that call consumes.

The compact inventory is a disposable request projection, not a new durable format. It has a 768-token approximate byte budget; oversized inventories retain a stable sender prefix with an explicit prose notice that additional senders are omitted and unfiltered `check_mail` still includes their mail. No omission counters or other model JSON metadata are added. Full canonical inventories retain notification IDs, receiver UUIDs, sender keys, and sequence watermarks for recovery. Only exact trusted canonical inventory artifacts are projected; ordinary marker-looking JSON is not rewritten. Sampling, local compaction, and newer remote compaction use this projection; legacy remote compaction retains its existing canonical-input behavior. Projection neither acknowledges mail nor changes unchanged-pending idle/restart suppression.

The sender's `z` does not immediately wake or steer with its payload, but the receiver's idle-boundary inventory notification can wake it. It is therefore not an absolute “never wake” guarantee.

Coalesce inventory notifications. Unchanged unread mail must never repeatedly wake a thread. If mail is consumed before the notification boundary, omit the obsolete notification. Newly arriving mail can make another notification eligible, but must not generate a wake per message while a batch can be represented together. Ordinary user steer retains its existing priority and does not require a mailbox check.

Already scheduled ordinary queued work takes precedence over an inventory-only wake. Keep inventory eligibility pending until an idle admission opportunity; do not merge it into or reorder those queued prompts. If intervening work consumes the mail, cancel the obsolete inventory. Use the existing scheduler/admission boundary.

Notification acknowledgement is distinct from consumption acknowledgement. Persist which accepted-mail range has been durably presented in an inventory, using existing delivery acknowledgement where possible. Define interruption and resume recovery around that boundary so an admitted-but-unrecorded inventory is not lost and a recorded inventory does not repeatedly wake the receiver. No separate scheduler is needed.

Canonical inventory recording is the durable notification boundary, not a guarantee that inference completed or the model read the inventory. Idle/resume recovery of an already-recorded notification repairs its watermark without starting another turn, even if acknowledgement previously failed. Fresh admission may record, acknowledge, and sample its reserved inventory turn once. Reuse the notification UUID as that turn's identity. Unrecorded preparations remain eligible for recovery; cancel obsolete preparations only after canonical absence is verified under the receiver's durable recording boundary. Check pending messages at or before the fixed frontier directly—newer messages from the same sender must not conceal that the original inventory is obsolete.

TUI inspection should expose pending counts and senders without consuming mail for the model. The idle-wake mechanism must share existing turn admission rather than race another independently started turn.

## Outgoing sends and subscriptions

Do not reject an outgoing `send_input` merely because the caller has unread mail, and do not consume mail as a side effect of sending. A compact pending-mail advisory in an ordinary tool result may be useful, but is optional and must not replace successful send semantics.

Mailbox submission creates no automatic commentary/final subscription: there is no specific target turn to bind yet. The receiver replies explicitly through `send_input` when appropriate. Main can independently establish normal `c/f` subscriptions for subsequent task turns.

Normalize order-independent flags before checking mailbox compatibility: cancel `f` and `x` pairwise, then reject `z` combined with `c`, `m`, `q`, or a remaining effective wake. Thus `z`, `zx`, `zfx`, and `zfxx` select the same mailbox admission, while `zffx` is invalid. Extra `x` has no response-subscription meaning for mail. Repeated `z` is idempotent. Never silently turn mail into steer or queued-turn input.

Initially only payload-bearing `send_input` opts into the mailbox parser surface. Spawn, resume, close, and user-shell flags retain their existing alphabet; sharing the parser does not enable mailbox semantics on those operations. User composer mailbox admission will use its own explicit input mode. Preserve all existing non-`z` normalization.

## Persistence implementation boundary

Mailbox storage extends the existing `ThreadStore` boundary, with dedicated tables in the existing queue database for the local implementation. Ordinary queued-user-input tables and automatic V2 mailbox draining keep their existing semantics. Storage does not derive or restore runtime authority.

Acceptance uses a receiver-scoped idempotency key and immutable payload. Consumption uses the receiver thread, originating turn, and tool-call identity to claim a fixed ordered batch, including empty batches. Retrying that invocation recovers its original membership; new arrivals cannot join it. Concurrent checks and waits cannot claim the same pending message.

Receiver JSONL history and mailbox SQLite state have separate commit boundaries. Persist stable delivery identities with the claim, append structured input through the existing canonical history barrier, and acknowledge only after canonical-history verification. Recover ambiguous writes by verifying those identities and delivering only missing members, rather than claiming an all-or-nothing batch transaction. Reuse the repository's existing restart durability contract; this adds no power-loss guarantee.

Inventory acknowledgement uses a separate accepted-message watermark and stable presentation identity. It does not consume payloads. Compaction or rollback must not make acknowledged mail pending again, and full thread forks do not copy the source's live inbox.

## Authority and visibility

Apply existing directed/subtree send permissions at submission and before the first canonical insertion of a mailbox member's model input. Revocation before that insertion rejects delivery, with an auditable rejection and non-waking sender warning, consistent with queued-input rejection. User mail is not governed by child-to-parent messaging grants.

The first canonical context write, serialized with permission changes under the existing messaging transaction, is the admission boundary. If context was committed but its typed presentation or acknowledgement was interrupted, recovery finishes that previously admitted delivery without requiring a new grant. Reuse the exact receiver-owned, claim-bound prepared envelope and append only missing artifacts; do not re-render attachments or insert the context twice. A subsequent disable remains effective for new input and is never undone by recovery. A presentation without canonical context does not establish admission and still requires current permission before writing context. Terminal consumed or rejected members are not reintroduced after rollback or compaction. This uses the trusted canonical writer and reserved delivery identities, not payload text or a new approval ledger.

Runtime permission state, user authorship, canonical sender identity, and payload text remain separate. Reading mailbox history or an inventory does not grant send authority. Mail itself does not create lifecycle ownership, response subscriptions, or permission to resume/adopt another thread.

Restart must preserve configured messaging state rather than require routine reauthorization. Persist user-controlled send permissions as authoritative settings, bound to canonical thread identities, and restore those settings on resume. Do not derive permission from accepted mail, transcript text, or historical audit events. Restore permission separately from transient turn-specific commentary/final subscriptions and consumed wake reservations.

Pending mail remains pending across restart; restart alone neither rejects it nor grants additional access. Explicit permission changes still govern consumption and produce auditable rejection where specified. Apply explicit lifecycle rules to close, delete, and adoption, retaining rejection audit information when a sender is unloaded rather than relying only on a live warning.

Accepted, pending, consumed, and rejected states should be distinguishable in presentation. Do not label merely accepted mail as already visible to the receiver model. Preserve full payloads in durable records; configured TUI previews remain presentation-only.

Agent mailbox-send tool rows use explicit structured mailbox metadata and display “Saved to mailbox for …” on successful acceptance, with distinct failed/interrupted wording. They do not show commentary/final subscription labels: mailbox acceptance creates no such subscription and does not establish consumption. Legacy tool-call records without this metadata retain their existing presentation. Incoming agent messages use the standard bullet and hanging indentation; nickname colors are deterministic and shared with lifecycle headers, whose task paths come from known agent metadata.

## Shared app-server and cross-root communication

Use the existing thread store and a shared host app-server rather than introducing a separate filesystem mailbox service. The thread store owns durable mail and consumption state. The app-server hosts and exposes the control plane; model tools and client requests share the existing Core/runtime recipient resolution, permission transactions, session queues, input admission, and inventory scheduling. Do not duplicate enforcement in an app-server-only path or relocate the whole runtime into app-server. `/agents` provides discovery and user communication controls. The agent graph continues to represent orchestration ownership, not every communication relationship.

Extend the same `send_input` interface across roots. Immediate and queued delivery retain their existing semantics; `w:z` deposits mailbox input. Cross-root communication requires an explicit user grant, independent of response subscriptions and graph membership. Sending does not adopt the recipient or grant interrupt, close, or other lifecycle authority. Pending mail for an unloaded thread remains durable until it is loaded.

### Resume without adoption by default

Cross-root `resume_agent` loads the existing thread under its existing root and configuration by default. It does not transfer ownership or implicitly grant communication permission. Loading through this operation remains subject to the applicable user-authorized access; a send grant alone is not a general lifecycle grant. Once loaded, pending mail follows the inventory-wake policy.

Adoption becomes explicit opt-in, used only when the user intends to transfer a thread into another orchestration tree. It is not required for loading, messaging, or shorthand addressing. This intentionally changes the current foreign-thread resume/adoption default; update model-tool contracts, TUI help, API documentation, and callers together. The exact opt-in field or syntax remains an implementation detail, but omission must not adopt.

Explicit adoption retains the existing ownership-transfer checks and task-path collision/remapping behavior. Non-adopting resume must not create parent edges, remap task paths, or inherit the caller's role/model/settings merely because the caller loaded the thread.

### Global root names

Use `/rename` to claim a unique root-thread shorthand in the shared host thread-store namespace. The latest explicit rename claims the name; ordinary activity, loading, or resume does not reclaim it. Assigning an already-owned name atomically clears the previous root's name and assigns it to the new root. Present a visible displacement notice identifying the previous owner. Historical records remain unchanged.

UUIDs remain canonical durable identities. Resolve shorthand names to UUIDs when accepting operations; subsequent renames cannot redirect accepted or queued messages, mailbox contents, or permission grants. Root-scoped refs, nicknames, and task paths remain local selectors, distinct from global root names. An explicit selector such as `thread:sepa-review` should disambiguate global addressing; finalize spelling and name comparison rules before implementation.

Do not maintain another parallel alias service or infer ownership from names. Integrate unique-name claims and lookup with existing thread naming/storage. Scope the guarantee to the shared host store; merging independent Codex-home stores is not implied.

For migration of pre-existing duplicate names, the most recent thread-name assignment wins and previous owners become unnamed. Use recorded name-assignment recency, not ordinary thread activity; where legacy records cannot establish assignment order, document a deterministic fallback before applying migration. Only explicit rename operations create subsequent claims; metadata synchronization and historical replay are not claims. Atomic displacement must update both owners and the applicable compatibility projections so legacy name-index fallback cannot resurrect the cleared name. Settle name comparison alongside selector spelling, without introducing another alias authority.

### Full user forks retain distinct names

A full user fork of a named source receives `<source-name>-N`, choosing the first available positive integer suffix in the destination naming namespace. The original name remains with the source; automatic fork naming must never use explicit-rename displacement semantics. Reserve the destination name atomically with fork creation so concurrent forks cannot claim the same suffix.

Apply this to full source forks within one Codex home and across homes via `--source-home`. Cross-home creation also uses a suffix even when the unsuffixed source name is free in the destination. Inspect destination collisions without writing to the source home. Forking an unnamed source remains unnamed. This rule concerns user full-history forks, not generated subagent nicknames or semantic task paths.

The September 8 read-only audit is available locally at `/tmp/codex-name-conflicts-2026-09-08.json`. It found 11 exact explicit-name conflict groups across 201 named records in `.codex`, `.codex-iremizov`, and `.codex-office`, all involving distinct thread IDs. It is an inspection artifact, not a migration: index-only existence, exact rename chronology, and fork ancestry require confirmation before modifying existing records.

## Integration and regression coverage

- Several explorers finish during delegation; Main receives only a coalesced inventory after finishing its turn and consumes selected senders in its chosen order.
- Active and idle recipients, new arrivals during consumption, empty checks, and unselected mail retention.
- Unchanged unread mail does not cause a notification loop; already-consumed mail does not cause an obsolete wake.
- User mail preserves text, attachments, authorship, and styling; agent mail preserves trusted attribution.
- Normal sends, steer, queue, and existing `c/f/m/x` subscriptions retain their behavior and do not implicitly drain mail.
- `wait_agent` consumes only targeted senders' mail addressed to the caller, returns on pending/new mail without claiming completion, and preserves existing completion/timeout behavior.
- Concurrent `check_mail` and targeted waits share consumption acknowledgement, preventing duplicate injection while leaving unrelated sender/user mail pending.
- Foreign sender selection consumes authorized incoming mail without granting lifecycle control or exposing unrelated execution/final-response data.
- Ordinary queued work precedes inventory-only wakes; consumption during that work cancels obsolete inventory eligibility.
- Interrupted/retried consumption and inventory delivery recover at their separate durable acknowledgement boundaries without losing mail or creating repeat wakes.
- Permission revocation before consumption rejects delivery visibly without an unintended wake.
- Durable pending/consumed state survives supported cold resume and history reconstruction without silently restoring messaging grants.
- Rollback, paginated revert, compaction, and fork paths preserve auditability without replaying consumed mail or copying a source thread's live inbox into another thread.
- Cross-root resume defaults to loading with original ownership/settings; explicit adoption alone transfers ownership and applies task-path remapping.
- Cross-root communication grants do not imply lifecycle control, and loading a recipient does not grant permission to send.
- Concurrent global-name claims have one atomic winner; displaced roots become nameless with visible notification. Resume/activity cannot reclaim a name.
- Renaming after input acceptance cannot redirect delivery or transfer UUID-bound grants; global selectors do not collide with local refs/task paths.
- Duplicate-name migration keeps the most recent name assignment and clears older owners across authoritative and compatibility projections.
- Full user forks allocate unique `-N` names atomically within or across homes, preserve the source name/home, and leave unnamed sources unnamed.

Before implementation, finalize the concrete persistence/admission acknowledgement boundary and retry identity, structured delivery/result shape, pending-mail treatment when grants disappear, inventory acknowledgement recovery, normalized `z` compatibility and eligible operations, cross-root load/status authorization, and global-name comparison plus legacy timestamp fallback rules. These decisions must fit existing thread-store and control-plane ownership rather than infer authority from historical messages.
