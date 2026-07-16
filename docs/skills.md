# Skills

For information about skills, refer to [this documentation](https://developers.openai.com/codex/skills).

Explicit skill mentions (for example, `$unwrap`, or a structured skill selection) are processed both at turn start and when accepted steering input is drained during an active turn. Before the next model request containing that input, the skills extension durably records a developer instruction block that promotes a resolved explicit-only skill into the thread's available inventory. Steering input rejected by hooks does not trigger promotion.

Promotion uses the existing provider catalog and retains the skill's authority, package, and resource identity; it does not turn executor or orchestrator resources into host filesystem paths. Repeating an unchanged promotion within a turn does not append another inventory update. Replacement inventories on subsequent turns include the currently available promoted entries. Catalog budgets still apply: omitted entries stay recorded as promotion metadata but are not acknowledged as visible, and can appear when the budget or available catalog changes.

Promotion is acknowledged only after the complete contribution has been appended, writer-flushed, and installed in live history. Cancelling a caller after publication starts does not cancel that owned publication; an ambiguous persistence failure requires canonical reload rather than appending the same batch again. Writer flush does not imply fsync or power-loss durability.

Compaction preserves the latest canonical inventory, and cold resume restores its bounded identities only from a host-classified `skills.catalog` developer item. An explicit empty inventory supersedes earlier promotion metadata. Restoration revalidates current provider authority and availability: an unavailable skill remains recorded without advertising a broken resource route. A quoted tag, user message, or client-authored developer message cannot supply promotion authority.

Host skill catalogs keep canonical paths for resource identity, authority, and reads, while exposing the configured discovery path as the model-visible display path. Display paths may preserve symlinked skill roots, contract paths below the host home directory to `~/`, and use forward slashes; these presentation changes never change the canonical resource route.
