# Continue an interrupted turn

Use `/continue` in an idle TUI to resume the latest interrupted or unfinished regular turn. After a harness closure, first reopen the saved session, then use `/continue`. The command sends `turn/continue`, not a new user message: Core resumes Responses sampling with the original turn ID, recorded input, output schema, service tier, and parent/root attribution.

The command does not resume an old network stream or promise server-side `previous_response_id` reuse after process death. It rebuilds the request from the existing conversation using the ordinary Responses client, without inserting a continuation prompt.

## Context and history

Only typed, receiver-owned harness interruption notices for that turn are omitted from model context. Their IDs are recorded in `TurnContextItem.recovery_start_options.cleared_interrupt_message_ids` before startup contributors, compaction, or sampling can run. The same edit is applied during cold reconstruction. The original abort event and messages remain in append-only audit history.

Actual user text, earlier turns, inherited messages, completed tool calls, and tool outputs are retained. In particular, a tool result reporting interruption or partial execution is not removed: it may be essential evidence about an external effect. Already recorded tool calls are not dispatched again merely because the turn continues. This is not an exactly-once guarantee if the model subsequently requests the same operation again.

## Admission

- A running or already-admitted turn is not duplicated. Queued prompt/trigger work is not consumed as a continuation request.
- Completed, failed, budget-limited, replaced, review, realtime, inherited, and rolled-back turns are not eligible. An ordinary user interruption is eligible; an unfinished turn after harness loss is eligible when its canonical input and settings were saved.
- Unresolved or ambiguous executable tool outcomes require reconciliation. Replay-generated placeholder outputs are not proof that an operation finished.
- Original model/settings and execution permissions must still match. The current durable recovery certificate covers one thread-configured local environment; remote or multiple environment identities are not reconstructed implicitly.
- Settings changes or newer work supersede an outstanding continuation. Unknown persistence outcomes quarantine the session rather than permitting another blind append or model request.
- TUI composer drafts, attachments, and deferred follow-up messages remain under normal user control. `/continue` does not automatically resume subagents; owning-tree restoration after local daemon loss remains a separate recovery path.

The app-server method is serialized with other requests for the same thread. It returns the original `turnId` after Core admits continuation; subsequent `turn/started` and normal streaming events describe the continued work. A rejected request adds no user input.

## Validation

Behavioral fixtures cover live interruption and cold restoration, both Legacy and Paginated history, retained tool results, repeated interruption/continuation, active/completed rejection, public RPC success and lifecycle notifications, and UI control dispatch/draft preservation. Generated protocol/schema artifacts and executable validation must be produced by remote CI; local formatting and static checks do not establish that these tests pass.
