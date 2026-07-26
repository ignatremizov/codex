# Codex Fork Ledger

This is the living maintenance map for capabilities carried by `fork` beyond its audited upstream integration base. It is a capability inventory, not a chronological changelog or a requirement to edit documentation in every feature commit.

The 0.160 integration is pinned to upstream source commit `cb7799623b2241f536d799f2d46259148fb987ce`, including its maintenance backports. The upstream release-notes/version commit `a956835d020762cb2b570053af06f643a11c0ecc` is deliberately excluded. Neither rolling `upstream/main` nor local `main` defines the integration base. Keep release-version changes at the final release owner.

The replay manifest inventories the 130 original downstream commits in `f1b21bb2931f86819e32df0cafa1bf69570d84ee..ac0f63abe470b8fa83d7852db7e5d39282c88434`. Reviewed dispositions distinguish retained owners, target adaptations, and explicitly deferred repairs assigned to their defining feature. After integration completes, inspect the resulting downstream stack with `git log --reverse cb7799623b2241f536d799f2d46259148fb987ce..fork`; during replay, `fork` still names the original branch tip. The complete pre-rebase ledger remains the policy-preservation checklist; a partially replayed foundation is not evidence that its later capabilities were dropped or that the new source has passed CI.

Use exact semantic commit subjects as ownership anchors rather than commit hashes. Refresh those anchors after splitting, squashing, or rewording their owning commits. Describe each capability's purpose, implementation entrypoints, and upstream integration seams so its ownership remains understandable after rebasing.

## Maintained Capabilities

Capability checkpoints are added as the fork develops; this foundation does not claim that later features already exist.

### Built-in collaboration role schemas

The collaboration tool plan exposes `agent_type` whenever built-in or configured roles are
resolvable. V2 collaboration tools retain their fork-owned bundled parameter schemas, encrypted
argument markers, namespaces, and runtime forwarding even when model catalogs provide incompatible
parameter overrides; catalog descriptions remain independently overridable where supported.
Entrypoints are `core/src/agent/role.rs`, `core/src/tools/spec_plan.rs`, and
`core/src/tools/multi_agent_tool.rs`. Focused schema and request-level coverage lives in the
corresponding core tool tests and scenario fixtures.
Ownership anchor: `fix(multi-agent): expose built-in roles and retain authoritative runtime schemas`.
Upstream's independent direct-message disablement and wait-tool gates remain authoritative.

Explicit child model overrides resolve against the loaded catalog regardless of the selected
multi-agent runtime tag; only the concise picker description is limited to five visible models.
Unknown models remain rejected with the complete loaded catalog listed, while child runtime exposure
continues to follow the resolved turn version.
Ownership anchor: `fix(multi-agent): resolve child model overrides across catalog runtime tags`.
The selection/validation owner is `codex-rs/core/src/agent/child_config.rs`; resolved runtime exposure
stays in `codex-rs/core/src/tools/spec_plan.rs`, independently of the description's display cap.

### Readable audit alongside V2 delivery

Ownership anchors: `feat(config): select the multi-agent V2 message delivery policy` and
`feat(multi-agent): separate readable audit from V2 message delivery`.
`core/src/tools/handlers/multi_agents_v2/message_tool.rs` validates configured opaque/audit/plaintext
payloads before target restoration; `core/src/agent/types.rs` preserves their typed representation.
The local runtime's registry and captured-thread submission path serialize accepted assignments
without retargeting replacement runtimes. Readable assignment metadata is volatile and separate
from canonical encrypted recipient input, human authorization, and durable mailbox ownership.
Runtime-aware tool-log redaction remains independent of upstream's name-only stream diagnostics.
Current direct-message disablement, capacity, root-target, residency, and queue-only/no-wake gates
are preserved. Executable and generated-schema qualification is still pending for this rebase.

### Authorized parent-history inheritance

Ownership anchor: `feat(multi-agent): require user authorization for inherited history`.
Model-authored V1/V2 spawns authorize full or bounded history only after resolving the selected
role. The global default is false, while a user-authored role may allow or deny that single
setting. V2's configured default selects history, not permission. Entrypoints are
`core/src/agent/child_config.rs`, `core/src/agent/role.rs`, and the two spawn-tool handlers.
Cold restores reapply recorded role policy without replacing the current runtime's permissions,
provider, or service-tier authority. Fork notification filtering changes only child context,
preserving canonical parent history and marking partially retained evidence incomplete.

### Durable inter-agent transcript provenance

Ownership anchor: `feat(multi-agent): publish durable inter-agent transcripts with typed provenance`.
The current detached history-publication worker owns canonical writer flush, prepared source/MCP
metadata, installation, and raw event delivery. World-state and confirmed-message publication
use that same ordering, while settings replies keep their independent pre-event acknowledgement.
`app-server-protocol/src/protocol/v2/inter_agent_message.rs` projects presentation provenance,
not human authorization or a new local assistant answer. Receiving-side ciphertext remains opaque.
Running resume retains a generation-aware per-connection snapshot/delivery boundary; cancelled
resumes require canonical retry. Paginated readers lazily rebuild fork-owned derived transcript
state under retained lineage reservations, without changing upstream migration versions or audit
bytes. TUI, CLI, exports, structured replies, and voice consumers distinguish these transcripts
from final assistant output. Generated-schema and executable qualification remain outstanding.

### Exact Legacy rollback and prompt identity

Ownership anchor: `fix(rollback): preserve exact durable Legacy thread boundaries`.
The Legacy `thread/rollback` compatibility route uses guarded canonical decoded-record boundaries,
single-attempt marker publication, shared submission admission, and exact-runtime quarantine when
commit or installation is uncertain. Paginated mutation remains `thread/revert`; neither rewrites
the user's working files. Retained terminal evidence, explicit checkpoint resume metadata,
count-only compatibility, and source-segment coordinates survive replay, forks, and migration.
Entry points are `core/src/session/rollback.rs`, `core/src/session/submission_admission.rs`,
`history/src/rollout.rs`, `rollout/src/recorder_barrier.rs`, and
`app-server/src/request_processors/thread_rollback.rs`. Derived projection versions are fork-owned
cache state, not amendments to released upstream migrations.
`tui/src/app/legacy_prompt_edit.rs` and `tui/src/app_backtrack/prompt_target.rs` bind edits to
canonical prompt identity, preserve drafts, order the canonical reset after transport events,
and keep an uncertain conversation read-only without disabling unrelated navigation or threads.
Recorder acknowledgement retains the file-flush contract, not a new fsync guarantee. Generated
protocol output and executable qualification remain pending for the completed rebase.

### Shared transcript Review and canonical navigation

Ownership anchor: `feat(tui): share concise transcript review and canonical target navigation`.
The existing `tui/src/transcript_view/review.rs` owns Review/Full mode and chronological targets
for both the owned viewport and inline overlay. Canonical message phase and structured patch
items supply navigation identity; full source, search, selection, voice ownership, and historical
Full previews retain their distinct contracts. Prompt edits require painted content and the
existing canonical rollback/revert safeguards. Copy-on-select and primary selection follow the
current client settings, and fullscreen session tips are not duplicated into transcript rows.
Entrypoints include `tui/src/app/owned_transcript.rs`, `tui/src/app_backtrack.rs`, and
`tui/src/pager_overlay/transcript.rs`. Snapshot and executable qualification remain pending.

### Fail-closed human command approval deadlines

Ownership anchor: `feat(approvals): enforce fail-closed human command deadlines`.
`core/src/session/command_approval.rs` and `request_command_approval.rs` own the monotonic
deadline, exact-turn generation, and one-use response claim. The private queued submission
retains upstream residency and trace/turn lineage while preserving rollback quarantine.
`app-server/src/command_execution_completion.rs` validates exact actor/listener/lifecycle receipts
after output backpressure; callback IDs alone never establish root-command authority.
The TUI retains the first receipt through buffering and replay, and expiry cannot approve work
or act on a later queued request. Absent timing remains untimed. Guardian, hooks, patch approvals,
and permission requests retain their separate policies. Human analytics uses structured trigger
fields, not opaque callback-ID presence. Generated contracts and executable qualification are pending.

## Maintenance Cadence

- Reconcile the inventory after each local release promotion and upstream rebase.
- Feature commits may update their rows when useful; a separate documentation checkpoint may consolidate multiple changes.
- Keep ledger maintenance separate from release version changes and unrelated runtime implementation.
- Include capabilities, compatibility choices, platform support, observability, and substantive efficiency improvements. Do not enumerate transient CI repair attempts.
- Remove or narrow a carry only after comparing its behavior with the upstream replacement.
- Record executable validation accurately; a history rewrite alone does not rerun CI or validate every intermediate commit.
