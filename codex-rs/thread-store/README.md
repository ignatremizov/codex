# Thread Store

`codex-thread-store` is the storage boundary for Codex threads. It defines the
`ThreadStore` trait plus local and in-memory implementations. Other storage
implementations may live outside this repository.

## Responsibilities

- `ThreadStore::append_items` is the raw canonical history append API. It does
  not infer metadata from item contents.
- `ThreadStore::update_thread_metadata` is the only thread metadata write API.
  It accepts a single literal metadata patch shape, regardless of whether the
  caller is applying a user/API mutation or facts derived above the store from
  appended history.
- `LiveThread` is the preferred API for active session persistence. It owns a
  per-thread metadata sync helper, applies the rollout persistence policy,
  appends canonical history, and then sends metadata patches through
  `ThreadStore::update_thread_metadata`.
- `ThreadManager` routes metadata mutations for loaded and cold threads through
  one entrypoint. Loaded threads use their `LiveThread`; cold threads go
  directly to the store.
- `LocalThreadStore` persists history through `codex-rollout` JSONL files and
  persists queryable metadata through the SQLite state database when available.
  Local explicit metadata mutations also maintain JSONL/name-index compatibility
  so reading old or SQLite-less local storage keeps working.
- `RolloutRecorder` is the local JSONL writer. It writes already-canonical
  items for `ThreadStore::append_items`; it no longer decides metadata updates
  for live thread-store appends.
- `core/session` creates or resumes `LiveThread` handles and does not need to
  know whether persistence is backed by local files or another store.

## Direction

New metadata observation semantics should live above `ThreadStore`. Stores
persist explicit metadata fields, but raw history appends remain history-only.

## Mailbox persistence

`ThreadStore::accept_mailbox_input` accepts immutable, receiver-scoped submissions
using typed user input (including client ID) or trusted agent attribution.
`claim_mailbox_input` reserves a fixed batch under receiver, turn, and tool-call
identity. Sender sets use canonical thread IDs, not mutable display names.
Retries preserve empty batches, membership, delivery IDs, and current terminal
states. Acceptance and claims do not grant permission, steer input, or start turns.

Local mailbox data uses the existing queue SQLite database. The in-memory store
mirrors acceptance, claims, and reconciliation within one process, without restart
durability. Ordinary queued submissions are unchanged.

`lookup_mailbox_input` reads accepted input by receiver and submission key,
including frozen attribution and the current state. An idempotent caller can
verify sender, sender turn, and original typed input and reuse the stored payload
after display metadata changes. Lookup does not claim, grant authority, or relax
acceptance's immutable byte comparison.

`reconcile_mailbox_delivery` accepts expected prepared model envelopes, not an
unchecked acknowledgement. For each claimed member it requires the reserved
`msg_mailbox_<delivery-id>` user-role model message and a receiver/turn-qualified
typed completion containing the original input and authorship. Prepared attachment
content is not compared to original attachment content. Missing proof leaves the
member claimed. Consumed and rejected members must not be injected again.

Verification reads canonical history once for the batch, using the existing
rollout reader and its plain/compressed representation support. It does not expand
fork source lineage or apply effective rollback/compaction filtering: historical
delivery remains delivery after removal from current context. Legacy and paginated
persistence both retain mailbox-specific typed completions alongside their model
envelopes.

Before physical paginated revert switches the current rollout, the local store
reconciles outstanding claims under the existing writer reservation. Claims with
no matching artifacts remain unchanged and do not block revert. Complete delivery
proof is acknowledged before cutover, so consumption survives the rollout switch
and restart. Partial or conflicting artifacts produce a recovery error and keep
the current rollout pointer unchanged; that error is not permission to resend.
Recovery never copies removed delivery content into the replacement model context.
This protects replacements made through this lifecycle; locating evidence made
unreachable by an earlier or external replacement is not implemented.

`recover_mailbox_delivery` returns each fixed member's exact persisted prepared
envelope and typed completion as absent, context-only, presentation-only, or
complete. It uses the same batch classifier as acknowledgement and pre-revert
recovery. It may establish a new fixed claim (including an empty one); callers
should claim first and recover with the same parameters, not a new invocation.
Partial artifacts do not authorize repair or resend. Terminal members are not
scanned and return `NotRecorded`; their terminal state remains authoritative.
Malformed canonical history fails recovery rather than being mistaken for absent
delivery. Existing non-mailbox completion readers retain their tolerant policy.
Valid copied metadata for a different thread with a future history mode is
ignored after receiver ownership is established; it is never rewritten or used
as delivery evidence. Potentially relevant or unclassifiable records still fail.

`reject_mailbox_input` delegates to the queue's terminal-state transition, retaining
content and fixed membership. Core must arbitrate delivery and permissions and
recover canonical artifacts before rejecting. Neither recovery nor rejection
grants authority, and callers never need direct access to SQLite.

Core must serialize canonical verification, append, retry, and live insertion for
the same invocation, and coordinate admission authority and rejection.
History append and SQLite acknowledgement are separate commits. A failure after
append but before acknowledgement is recovered by verifying history, not by
assuming an atomic exactly-once transaction across the two stores.

## Mailbox inventory

`lookup_mailbox_claim` reads a fully qualified receiver/turn/tool-call invocation
without establishing a claim. Missing invocations return `None`; existing empty
or terminal claims retain their original selection, membership, and delivery IDs,
with current member states. Retry callers must validate the returned selection.

`read_mailbox_inventory` reads pending and claimed sender counts separately.
`prepare_mailbox_inventory` fixes at most one active notification per receiver;
new arrivals do not join that preparation. Notification progress never consumes
mail. Both operations use the existing queue store, with process-local parity in
the in-memory implementation.

`has_pending_mailbox_inventory` validates the supplied notification snapshot and
checks actual Pending rows for its receiver at or before its frozen frontier.
Newer mail from the same sender cannot keep an obsolete snapshot eligible. This
read neither changes progress nor authorizes cancellation; a false result still
requires the checked cancellation path under durable delivery arbitration.

`MailboxInventoryNotification::context()` derives a developer-role inventory
message from its immutable canonical sender/count/frontier snapshot and fixed
`check_mail` guidance. The notification UUID is its turn ID; its distinct reserved
context ID is `msg_mailbox_inventory_<UUID>`. This is trusted harness context, not
user task input or sender-authored payload. Canonical UUID listings are a foundation;
human-readable refs and task names remain a presentation follow-up.

Under caller-held durable delivery arbitration, recovery verifies the receiver's
own canonical context ID, deterministic turn binding, and exact frozen snapshot.
It returns the recorded envelope with its original stamps. Reconciliation
acknowledges only this proof and returns SQLite's committed watermark directly,
without a post-acknowledgement read. Callers retain the original notification across
uncertain receipts. An old notification is `AlreadyCovered` only when its exact
canonical proof exists and the watermark covers its frontier; this path never
mutates a newer active notification.

Cancellation requires the matching active snapshot and proven canonical absence.
Callers must flush pending canonical writes and hold delivery arbitration through
the check and cancellation. Conflicting or unreadable history is not absence.
Pre-revert recovery checks active inventory in the same physical-history read as
mailbox delivery: recorded inventory is acknowledged before cutover, unrecorded
preparations survive unchanged, and conflicting proof prevents cutover. No inventory
payloads or recovery context are copied into replacement model history.
