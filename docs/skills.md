# Skills

For information about skills, refer to [this documentation](https://developers.openai.com/codex/skills).

Explicit skill mentions (for example, `$unwrap`, or a structured skill selection) are processed both at turn start and when accepted steering input is drained during an active turn. Before the next model request containing that input, the skills extension durably records a developer instruction block that promotes a resolved explicit-only skill into the thread's available inventory. Steering input rejected by hooks does not trigger promotion.

Promotion uses the existing provider catalog and retains the skill's authority, package, and resource identity; it does not turn executor or orchestrator resources into host filesystem paths. Repeating an unchanged promotion does not append another inventory update. Recorded promotions survive interruption and subsequent turns; a cancelled contribution is not acknowledged unless its context was durably recorded.
