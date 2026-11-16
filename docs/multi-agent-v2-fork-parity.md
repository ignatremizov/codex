# Compact V2 input and fork-feature parity

The 0.169 integration includes migration of the fork's V1 capabilities into V2. V1 remains supported until every applicable fork-feature obligation has executable evidence on the exact final candidate. A renamed tool, matching schema, or passing default-V1 run is not full parity.

The preservation baseline is `4ec3c37b36b68480c915e744be578840d33ff25b`, containing all 174 frozen source owners above stable code parent `fa266e73c414c1798f6ada3f85a151044e07f7c4`, plus 23 not-yet-folded preservation fixups. Subsequent migration changes have their own semantic owners. The original qualified source remains `175fecff94216709eedf654bb5f4f10daf62147d`.

## Shared contract foundation

`agent/input_policy.rs` owns the existing typed `w` policy and its parser. Omission is not explicit passive delivery: the controller resolves ancestor-directed defaults per receiver, and omission must not erase an existing explicit subscription. The parser delegates to the same protocol-level flag algebra for `c`, `f`, `x`, `q`, `z`, and conditional mailbox finals; retired `m` and invalid combinations still reject.

`tools/handlers/multi_agents_input.rs` owns target string/array syntax and the compact receipt projection. Logging keeps the canonical admission identifier, while direct model output and Code Mode both expose status and an optional actionable hint. Queued, accepted, consumed, and completed are different facts; formatting a receipt grants no authority and does not make uncertain input safe to retry.

The V1 handler and its single/batch admission adapters now use these shared definitions. This extraction changes no runtime defaults, public V1 fields, delivery behavior, or host capability. It is a prerequisite for the compact V2 adapter, not evidence that the V2 public tool surface or admission semantics have already migrated. Semantic ownership: `refactor(agents): share input policy and compact receipts across tool versions`.

## Explicit compact selectors through native V2

The native controller's public reference-resolution path and typed send/resume/interrupt target path now share the existing durable alias owner for explicit `ref:N`, `nick:Name`, `id:UUID`, and `task:path` selectors. The issuing root determines alias identity; selecting an alias does not load a closed runtime, adopt another root's agent, or grant permission to send, interrupt, resume, or detach. Each operation still applies its own lifecycle and authorization checks after resolution.

Unprefixed V2 names retain their existing runtime-relative path interpretation. Explicit selectors never fall back on a missing or denied lookup. V2 runtime paths retain their existing lowercase-letter/digit/underscore grammar, including rejection of dot traversal; selector prefixes do not overlap valid runtime path segments. Bare decimal refs and unprefixed nicknames are **not** newly reinterpreted as aliases in V2. Host-owned controllers continue to receive the original selector through their own `resolve` implementation; Core does not consult its local alias store around a host rejection.

This is an implemented part of V02, not full addressing parity. Source-authored integration cases cover native V1/V2 root scoping, closed-agent nonloading, unchanged V2 relative paths, two roots with the same compact ref, exact-ID ownership rejection, hosted resolution/revocation with no native fallback, and an encrypted/audited V2 message sent by compact ref. These cases require remote execution. Spawn/roster compact-reference presentation, complete unified `send_input` exposure, and measured full-envelope budgets remain separate work.

Semantic owner: `feat(agents): share explicit compact selectors with native V2 controllers`.

## Compact read-only roster status

Native V1 discovery and V2 roster projection now use one `CompactAgentStatus` conversion. The model-facing V2 `list_agents` output retains canonical agent names and complete readable task text, but uses the same small lifecycle labels as V1 instead of embedding final-response or error bodies in every roster refresh. Full backend `AgentStatus` payloads remain unchanged in canonical tool logs and the response/observation owners; discovery does not consume a final-response receipt or change agent ownership. Direct and Code Mode outputs use the same projection.

The advertised output schema describes those exact lifecycle labels. Source tests cover every status, complete audit payload retention, direct/Code Mode equivalence, unchanged V1 vocabulary, and a two-entry 512-byte model-envelope budget with large completion/error payloads. The test emits UTF-8 byte counts through tracing when executed; no token reduction or end-to-end latency saving is claimed before measurement. Complete task text is deliberately retained, so the bound applies to the specified fixture, not arbitrary roster tasks.

Semantic owner: `feat(agents): share compact roster status across V1 and V2`. This implements part of V11/V12 without changing roster visibility, host authority, wait semantics, messaging tools, or runtime defaults. Remote execution and final schema/snapshot checks remain pending.

## Parity inventory

The table records source-inspection status, not test results. An implementation can be present without being proven. During each port, add the actual owner and executed V1/V2 test receipts; any unproved applicable obligation continues to block V1 retirement.

| Area | Fork obligation | Current V2 status and next boundary | Evidence required |
| --- | --- | --- | --- |
| V01 — unified input | One `send_input` with scalar/batch targets, message or structured items, interruption, and `w`; exact native identity | Shared policy/receipt foundation extracted. Separate V2 `send_message` and `followup_task` still need a unified adapter over real admission. | Actual advertised schemas and direct/Code Mode dispatch; invalid calls reject before admission. |
| V02 — compact addressing | Root-scoped refs, nicknames, Main, UUID fallback, canonical task paths, and ambiguity rejection | Explicit native selectors share the existing alias owner through V2 controller APIs. Bare paths and host resolution stay distinct. Compact spawn/roster presentation and remaining parity require further work. | Native V1/V2 root-isolation, nonloading, encrypted delivery, and host no-fallback integration cases authored; remote results pending. |
| V03 — observations | Commentary/final subscriptions, explicit passive versus omission, and exact receiving-turn identity | Shared parser is available; V2 encrypted-message delivery still needs the same observation admission/commit boundary. | Started, steered, queued, unknown, cancellation, and persisted source-to-turn correlation in both modes. |
| V04 — queues | `q` target FIFO is distinct from ordinary, steer, and scheduled CLI input | Existing queue owner must accept the correct V2 message representation, not map `QueueOnly` to a distinct-turn queue. | FIFO, no implicit steer, source-next-turn final, and exact receipts. |
| V05 — mailbox | `z` receiver-owned durable admission, consumption, inventory-bound conditional final, and no invented read receipt | Native V1 tool currently guards this surface. V2 needs an explicit supported representation/backend path; unsupported backends must fail before mutation. | Cold/loaded, claim epochs, crash recovery, consumption, and `zf` final tests. |
| V06 — batch | Canonical recipient deduplication, per-recipient policy, partial outcomes, and safe cancellation | Shared target syntax and compact receipt are reusable; V2 batch dispatch remains to be ported. | Mixed recipients, independent default resolution, uncertain admission, and no blind retries. |
| V07 — lifecycle | Spawn/resume/close, owning-parent restoration, detach/reattach, provisional cleanup, and durable subtree unload | Existing V2 spawn/resume/interruption and shared runtime owners are retained. Additional fork behavior needs real V2-path coverage. | Loaded/cold authority, accepted-child ownership, duplicate IDs, drain, and writer release. |
| V08 — waits | Invocation-owned cancellation/deadlines, input wake without consumption, and no idle wake from passive finals | Existing V2 wait runtime is retained; shared input policies must not change its wake contract accidentally. | Active wait/sleep, cancellation, passive/explicit final, and task replacement. |
| V09 — settings | Independent model/effort/tier, roles, captured provider/auth/executor, and bounded/full inheritance | Existing fork V2 resume/model/role and source-authority changes are retained, not assumed equivalent merely by field names. | Requested/effective settings, foreign executor, managed restrictions, and future-turn persistence. |
| V10 — review and audit | Native hook identity, review before effects, post-hook action validation, and encrypted/audit pairing | Current V2 `EncryptedWithAudit` remains the default; compacting presentation must not replace it with plaintext or drop ciphertext/audit evidence. | All delivery modes, payload bounds, namespace aliases, Guardian denial, and opaque/readable separation. |
| V11 — history and UI | Canonical source/receipt IDs, compaction/rollback/resume, complete export, labels, and compact projections | Source-preserving V1 and V2 publication paths exist but differ; they need explicit convergence rather than truncating authored content. | Replay and rollback proofs, same-text distinct identities, full copy, and receiving-thread presentation. |
| V12 — questions/roster | Requester-bound answers, qN identities, opt-in read-only discovery | V1/V2 now share compact lifecycle-only roster status; V2 full audit and task text retained. Visibility and question capabilities remain separately scoped. | Status/audit/size and actual handler tests authored; requester and side-effect runtime checks still require remote receipts. |
| V13 — backend ownership | Native versus host control, host admission/accounting/outcomes, service tiers, and exact ownership epochs | `AgentControl` remains the backend boundary. A missing host capability cannot fall back to local V1 runtime or claim acceptance. | Explicit capability failures and supported local/host/mixed-tree/executor paths. |
| V14 — exposure/continuity | Configured namespaces, discovery, Code Mode, hooks, prompts, schemas, and inert historical compatibility | V1 stays selectable. V2 public tool consolidation and guidance propagation must be coordinated rather than deleting names alone. | Both actual modes, old-record replay without execution, generated contracts, and no duplicate public variants. |

## Envelope measurement

Capture actual serialized tool schemas, input/results, observation events, and representative encrypted/audited messages before and after V2 migration. Separate fixed framing from authored payload and required audit bytes. Record UTF-8 bytes; report token counts only with an identified tokenizer/model. Do not claim savings from source-code line counts or by dropping original content, ciphertext, receipt identity, or authority evidence.

The shared receipt regression asserts identical compact direct/Code Mode projections while retaining the full admission identifier in logs. Runtime V2 envelope budgets and comparative measurement are still pending; this document does not invent numerical savings.

The test-only `multi_agents_v2/envelope_baseline_tests.rs` producer captures the existing production tool factories and serializers before public input consolidation. Manual Verify exports its JSON evidence when the test runs, including the exact source SHA, complete fixture values, UTF-8 serialized lengths, authored-text bytes, required ciphertext bytes, and the remaining JSON framing/escaping bytes. Fixtures cover V1/V2 tool schemas, V1 submitted/queued/mailbox receipts, direct and Code Mode result projections, both current V2 message delivery modes under all three encryption configurations, 32-message repetitions, and V1 single/batch flag arguments. Dummy opaque fixture content measures framing; it is not real encryption or a cryptographic size benchmark.

This producer does not dispatch input or establish backend authority, receiving-turn correlation, latency, or token savings. Its report lists the still-unmeasured target-upstream baseline, actual spawn/resume receipts, response delivery, partial/unknown admission, receiver queue/mailbox behavior, and end-to-end namespace/Code Mode dispatch. Remote capture and review remain required before setting comparative budgets or claiming improvement. The producer's source presence is not a successful measurement receipt.

The same explicit capture option now runs an independent fork serializer
producer on the exact integration revision, without first requiring the full
workspace archive to finish. Its complete output and source/fixture identity
are retained for comparison to the pinned-upstream producer. Both captures
must pass when requested; neither qualifies actual receiving-turn admission,
mailbox/queue operation, or a complete V2 contract. These remain separate
full-test and runtime evidence gates.

An opt-in `capture_upstream_agent_envelopes` Manual Verify job prepares the
corresponding baseline against immutable upstream code parent
`fa266e73c414c1798f6ada3f85a151044e07f7c4`. It checks both checkout identities,
keeps the production implementation intact, and records the exact test-only
overlay. Its fixtures distinguish upstream direct encrypted messages from
trusted plaintext and Code Mode messages, preserve full payloads, and capture
unsupported fork-field parsing through the actual upstream parser. The initial
fork and upstream producers both retain explicit runtime-delivery gaps; neither
source presence nor a serializer-only pass establishes full parity. Remote
execution and artifact comparison are still pending.

## Qualification and retirement

Both V1 and the actual V2 runtime must be explicitly exercised. Preserve the complete fork-ledger feature inventory; the rows above are a minimum, not a reduced definition of parity. Report concrete remaining gaps after each coherent port. Only proven full parity, acceptable measured overhead, and exact-final unfiltered remote qualification can support a separately recorded V1 retirement/default decision. No automatic cutover or session migration is authorized by deprecation alone.
