# Continue an interrupted turn

Use `/continue` in an idle TUI to resume the latest interrupted or unfinished regular turn. After a harness closure, first reopen the saved session, then use `/continue`. The command sends `turn/continue`, not a new user message: Core resumes Responses sampling with the original turn ID, recorded input, output schema, service tier, and parent/root attribution.

The command does not resume an old network stream or promise server-side `previous_response_id` reuse after process death. It rebuilds the request from the existing conversation using the ordinary Responses client, without inserting a continuation prompt.

## Context and history

Only typed, receiver-owned harness interruption notices for that turn are omitted from model context. Their IDs are recorded in `TurnContextItem.recovery_start_options.cleared_interrupt_message_ids` before startup contributors, compaction, or sampling can run. The same edit is applied during cold reconstruction. The original abort event and messages remain in append-only audit history.

Actual user text, earlier turns, inherited messages, completed tool calls, and tool outputs are retained. In particular, a tool result reporting interruption or partial execution is not removed: it may be essential evidence about an external effect. Already recorded tool calls are not dispatched again merely because the turn continues. This is not an exactly-once guarantee if the model subsequently requests the same operation again.

An interrupted tool call without a saved result does not block continuation. Both `/continue` and automatic recovery after daemon loss resume sampling through ordinary prompt normalization: missing function, custom-tool, and shell results receive the existing `aborted` placeholder, and missing client tool-search results receive an empty result. These placeholders stay in model context and do not become saved evidence that the tool finished. The model can inspect the situation and request another call, including a new sleep; surviving processes or external effects may still need checking before retrying an operation.

## Admission

- A running or already-admitted turn is not duplicated. Queued prompt/trigger work is not consumed as a continuation request.
- Completed, failed, budget-limited, replaced, review, realtime, inherited, and rolled-back turns are not eligible. An ordinary user interruption is eligible; an unfinished turn after harness loss is eligible when its canonical input and settings were saved.
- Missing tool outcomes permit continuation; ambiguous or unidentified tool-call identities still require reconciliation.
- Original model/settings and execution permissions must still match. The current durable recovery certificate covers one thread-configured local environment; remote or multiple environment identities are not reconstructed implicitly.
- Settings changes or newer work supersede an outstanding continuation. Unknown persistence outcomes quarantine the session rather than permitting another blind append or model request.
- TUI composer drafts, attachments, and deferred follow-up messages remain under normal user control. `/continue` does not automatically resume subagents; owning-tree restoration after local daemon loss remains a separate recovery path.

The app-server method is serialized with other requests for the same thread. It returns the original `turnId` after Core admits continuation; subsequent `turn/started` and normal streaming events describe the continued work. A rejected request adds no user input.

## Validation

Behavioral fixtures cover live interruption and cold restoration, both Legacy and Paginated history, retained tool results, repeated interruption/continuation, active/completed rejection, public RPC success and lifecycle notifications, and UI control dispatch/draft preservation. A test-owned process is killed after persisting a sleep call without its result; explicit continuation and automatic tree recovery must both reach the model with a prompt-only placeholder under the original turn ID. Selector and normalization fixtures cover missing function, custom-tool, shell, and client tool-search results. Executable validation must be produced by remote CI; local formatting and static checks do not establish that these tests pass.
