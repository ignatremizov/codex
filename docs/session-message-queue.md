# Messages to existing sessions

For delayed input and headless execution that can load a thread, see
[scheduled thread execution](scheduled-thread-execution-spec.md).

Send a message using a thread UUID or an exact active session name:

```sh
codex queue --thread <thread-id-or-name> --message 'Continue with the next task.'
```

The message stays in the durable queue until the thread finishes its current turn
and can start another. A running `clock.sleep` is part of the current turn.

To deliver into the current turn and wake input-interruptible waits, use:

```sh
codex queue --steer --thread <thread-id-or-name> --message 'The review is ready.'
```

A steer can pass messages waiting for the next turn without consuming them.
If the target is idle, the message starts a new turn. Unsupported active tasks
retain the steer for a later attempt. An explicitly interrupted or unloaded thread
keeps the message until normal resume; sending alone does not resume or adopt agents.
Use `--resume` to request a best-effort resume of an unloaded thread after durable
acceptance, through the same local daemon or selected remote server. If that
resume fails, the command reports a warning and the accepted message remains
queued. The existing restriction on direct input to unloaded spawned subagents
applies.

The receipt confirms acceptance into durable storage. It does not confirm that
the model has read the message. Do not automatically retry after an uncertain
transport failure.

Local account profiles sharing the same state directory share the queue database.
The CLI connects through its selected auth profile, and the process hosting the
target thread delivers the message using that thread's own credentials and
execution environment. The UUID does not select a server. An exact session name
must resolve unambiguously in the selected server's inventory.

Delivery to a thread in the receiving server can happen immediately. Other
servers discover changes through the existing poll, normally within 10 seconds.
This is active-turn delivery, not a strict latency guarantee. Separate state
directories and remote hosts have separate queues; `--remote` selects an explicit
server using the existing connection and authentication rules.

`--steer` uses the experimental `thread/queue/steer` RPC with the same parameters
and acceptance receipt as `thread/queue/add`. Older receiving servers reject the
method instead of silently queuing a next-turn message. Upgrade the target's
hosting server too: an older queue reader can retain ordinary queue compatibility
but cannot wake an active turn for a steer.

Schedule ordinary text without keeping a shell or conversation loaded:

```sh
codex schedule --thread <thread-id-or-name> --at 'in 2h' --message 'Check the results.'
codex schedule list --thread <thread-id-or-name>
codex schedule result <input-id> --wait --json
codex schedule cancel <input-id>
```

The TUI accepts `/schedule 17:30 <prompt>`, `/schedule in 2h <prompt>`, and the
same `list`, `cancel <id>`, and `result <id>` actions. Pending schedules appear
above the composer with their resolved date, time and offset. A bare time means
the next occurrence in the submitting client's timezone; an ISO timestamp must
include its offset. Explicit past times are immediately due.

The loaded owner delivers due input using its own account. Otherwise, a live
server matching the saved fallback auth profile resumes the same UUID. Shared
and embedded owners use the same queue. If no eligible server is alive, the input
waits; scheduling does not boot the host or start a service after the CLI exits.
Future input does not hold a session, agent slot, fake subscriber or writer lock.
Normal inactive/unsubscribed cleanup remains in force after execution.

Queue edits preserve the due time and fallback profile; deletion cancels pending
input, including after the target is archived. Once due dispatch begins,
deletion is best effort and the input may still
run. Explicit unload stops current work while preserving future input. Admitted
work interrupted by unload or server death is not automatically submitted again.
Archived or missing targets error, and unloaded spawned agents require normal
tree restoration.

Creation returns an input ID. If its reply is lost, the error includes the
client-generated ID: inspect `result` or `list` before recreating it. Receipt
lookup identifies the actual receiving turn, including a steer, rather than
waiting for an unrelated completion. `result --wait` observes that turn; `-o`
writes its final answer. Closing observation does not cancel pending input.
Creation with `--wait` and shared exec resume attempt an ordinary interrupt on
Ctrl+C only if their input started the turn. A different hosting server may
require interruption through its own controls.

Deploy participating shared servers and embedded TUIs together before using
scheduling. The new experimental APIs require regenerated protocol bundles and
remote CI qualification; source changes alone do not update installed binaries.
