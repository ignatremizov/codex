# Targeted mailbox waiting

V1 `wait_agent` retains its `targets` and `timeout_ms` arguments and its `status`
and `timed_out` result fields. `return_reason` distinguishes `mail`, `completion`,
`timeout`, and `recovery`. Completion takes precedence when both a terminal status and
selected mail are ready.

Direct calls select mail addressed to the caller from the union of the resolved
target UUIDs. Existing pending mail can return immediately. New mail wakes the
wait through a receiver-local activity signal; the wait then reads durable
inventory again. Unrelated senders and user mail do not satisfy the selection or
reset the deadline. Queued prompts and steered input are not consumed.

Every successful direct return carries a mailbox consumption request. New
invocations capture the selected pending batch, including completion and timeout
returns. The accepted tool result is recorded
before the selected messages are delivered as separate structured input. The
result contains no message bodies or guaranteed delivery count. Another consumer
may claim previously observed mail, and delivery still checks current permission.
The fixed claim at the consumption boundary preserves acceptance order; subsequent
arrivals remain pending. A retry uses the same invocation's fixed claim rather
than selecting a replacement batch.

A direct retry first looks up the invocation's existing claim without creating one.
An existing claim returns immediately with `return_reason: "recovery"` and
`timed_out: false`, even when the fixed batch is empty or its members are already
claimed or terminal. The original sender selection must match; a changed selection
is an error. This path neither waits for new messages nor repeats completion
presentation arbitration. If the original tool result was already recorded, the
ordered recorder reuses that exact canonical result. Otherwise, `recovery` describes
the repair without inventing a completion, mail arrival, or elapsed timeout.

Refs and task selectors retain caller-root resolution. Direct calls can also
select a sender by canonical UUID, including `id:<UUID>`, without adopting or
loading that sender. UUIDs without status authority appear in the optional
`mail_only_targets` array. Their status is not observed: no foreign final content
or fabricated `Completed`/`NotFound` value is inserted into `status`. Controlled
targets retain their normal terminal-status behavior and original selector keys.
Mail selection does not grant permission to send or alter cross-root policy.

Nested and code-mode calls retain their existing controlled-target resolution,
errors, and completion/timeout waiting. They do not return for mailbox activity,
claim messages, or drain mail; ordered mailbox effects are not supported by the
outer nested-result recorder. They also return `return_reason`, but do not gain
foreign mail-only target support. Existing tool exposure is unchanged.

Waiting preserves the existing targeted completion-presentation arbitration and
does not create a response subscription. Inventory checks are event-driven, not
database polling on a timer.
