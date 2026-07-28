# TUI background subagent completion rendering

A child result can appear in its parent's transcript as `Agent finished` without requiring an active `wait_agent`. The row reports completed, errored, shutdown, or not-found status. It is a child result, not the parent's final answer.

## Presentation and replay

Background rows and completed waits use the same per-agent detail renderer. They preserve multiline text, error details, local response-preview limits, and hidden-row markers. The complete source remains available to raw transcript copy and export even when the visible preview is capped. Live notifications, resume replay, and historical transcript pages use that shared renderer.

The TUI does not decide which operation owns completion presentation. Core captures a terminal presentation token for the child run. A wait can claim only the terminal tokens included in its frozen result. A later wait remains a separate event and can display the same result again; this is not a duplicate background delivery. There is no text-based deduplication.

Canonical background rows bypass parent assistant-answer bookkeeping. Attributed inter-agent messages continue through their existing communication renderer. Review/Full transcript behavior, canonical user identities, prompt-edit rollback fences, and approval countdown receipt times are independent of these rows.

## Provenance boundary

Core completion items carry private typed provenance. The core-to-app-server conversion preserves a reserved completion identity only after validating that provenance against the phase, status, and envelope. Reserved-looking ordinary provider IDs are normalized out of that namespace. Raw inter-agent conversion does not promote an attributed message into a canonical background completion, and context-only completion messages are omitted from the public transcript.

The public wire shape does not expose the private completion metadata or wait ownership fields. The TUI decodes the reserved identity, commentary phase, and envelope supplied by that trusted conversion. Its direct core-item path first validates the typed predicate. Text resembling a completion envelope is never sufficient provenance. Public wait rows likewise do not prove ownership: ownership is recorded in the private canonical item.

V1 notification context and v2 mailbox context keep their respective delivery paths. Reserved context identities remain paired with core-authored delivery metadata so reconstruction and exact rollback need not classify message text. Accepted completion context and presentation artifacts are retained across supported exact rollback. Paginated rollback remains unsupported.

## Canonical persistence and delivery

Completion work is bound to the exact parent Session instance that accepted it. A resumed Session with the same thread ID is not an interchangeable destination. Shutdown closes admission and, on a healthy instance, drains earlier accepted completion work before closing persistence. This does not automatically restore or rebind a closed agent.

Canonical completion persistence uses the existing writer and an acknowledged flush barrier before primary lifecycle delivery. This is a writer-flush contract, not an fsync or power-loss guarantee. A batch may commit a prefix; it is not a transactional all-or-nothing append. Secondary metadata and paginated projections may lag a successful canonical receipt.

A completion receipt distinguishes canonical persistence from enqueueing the primary event. Enqueueing means acceptance by the core event channel, not that an app-server client or TUI has observed or rendered the row. A wait commits presentation ownership only after both the canonical barrier and successful primary enqueue. A closed event channel does not count as successful delivery.

An uncertain write quarantines the affected runtime and requires recovery. It is not treated as a transparent retry, and a read after an error does not manufacture a successful receipt. Stable identity reconciliation requires the expected canonical turn and matching item; it never falls back to text matching or an unrelated turn. A recovery barrier on a new exclusive healthy runtime does not clear uncertainty on the old instance. Cold replay preserves existing durable records; it does not repair missing lifecycle suffixes or reconstruct earlier enqueue acknowledgements.

These guarantees begin when the parent has accepted and durably recorded the relevant work. They do not guarantee delivery after a process crash before parent recording. Uncertain teardown remains fail-closed.

## Coverage

The TUI scenarios cover terminal status snapshots, live/resume/cold-page parity, local preview authority and full raw source, direct core-item provenance, forged reserved IDs without typed metadata, attributed messages and wrong phases, parent answer bookkeeping, and a later wait rendering a separate row. Core, history, and app-server suites cover persistence, ownership, projection, shutdown, and reconstruction at their respective boundaries. Test execution is reported separately from the presence of this coverage.
