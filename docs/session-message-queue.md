# Messages to existing sessions

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
keeps the message until normal resume; sending does not resume or adopt agents.
The existing restriction on direct input to unloaded spawned subagents applies.

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
