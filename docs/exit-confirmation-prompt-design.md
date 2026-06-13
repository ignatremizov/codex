# Exit and shutdown flow (tui)

This document describes how exit, shutdown, and interruption work in the Rust TUI (`codex-rs/tui`).
It is intended for Codex developers and Codex itself when reasoning about future exit/shutdown
changes.

Exit gestures are handled by the chat widget and the app-server-backed `App`.
The file retains its historical design-note name, but the behavior below follows
the current implementation. Earlier shutdown and confirmation designs are not
durable-cleanup guarantees for this implementation.

## Terms

- **Exit**: end the UI event loop and terminate the process.
- **Disconnect cleanup**: stop realtime, dispose of UI-owned side conversations,
  and unsubscribe the current thread through the app server.
- **Interrupt**: request cancellation of the current turn through the app server.
  Interruption is separate from leaving the client or stopping a shared server.

## Event model (AppEvent)

Exit is coordinated via a single event with explicit modes:

- `AppEvent::Exit(ExitMode::ShutdownFirst)`
  - Request disconnect cleanup before leaving the UI.
- `AppEvent::Exit(ExitMode::ShutdownAfterInterrupt)`
  - Use the same cleanup path after a successful explicit turn-interrupt request;
    return the `TurnInterrupted` exit reason.
- `AppEvent::Exit(ExitMode::Immediate)`
  - Stop realtime and leave without the ordinary unsubscribe/side-thread cleanup
    sequence. The daemon's "run in background" choice uses this path.

`App::handle_exit_mode` resolves outstanding TUI dynamic-tool requests as failures
before handling the exit mode. For the shutdown modes, it gives
`shutdown_current_thread` a two-second UI escape budget, logs a timeout, and then
exits. This timeout is not proof that backend work, persistence, or subprocess
cleanup completed.

## User-triggered quit flows

### Ctrl+C

The built-in double-press quit shortcut is disabled. At the chat-widget layer:

1. Active modal/view gets the first chance to consume (`BottomPane::on_ctrl_c`).
   - If the modal handles it, the quit flow stops.
   - Composer draft cancellation can also consume the key.
2. The agents overview can request exit when it does not consume the key itself.
3. Cancellable work is interrupted; otherwise the widget requests shutdown-first
   exit without a confirmation timer.

The app layer can intercept daemon-backed running-task gestures to show choices
such as canceling work, running it in the background, or interrupting and exiting.
Side conversations also have their own close/return-to-parent behavior. Do not
treat the chat-widget fallback as the whole daemon exit policy.

### Ctrl+D

- Only participates in quit when the composer is empty **and** no modal is active.
  - Requests shutdown-first exit immediately; no second press is required.
- With any modal/popup open, key events are routed to the view and Ctrl+D does not attempt to
  quit.

### Slash commands

- `/quit` and `/exit` request shutdown-first exit without a confirmation prompt.
- `/logout` first requests account logout. Success enters the shutdown-first
  exit path; failure is displayed and leaves the UI running.

### /new

- Opens the new-session checkout flow. This is a session transition, not a
  promise to terminate the app-server process.

## Cleanup ownership and implementation

- `chatwidget/interaction.rs` owns the Ctrl+C/Ctrl+D fallback behavior.
- `app/event_dispatch.rs` owns exit-mode handling and running-task exit choices.
- `app/thread_routing.rs::shutdown_current_thread` stops realtime, closes side
  conversations, attempts `thread/unsubscribe`, and retires the local listener.
- Realtime stop has its own one-second timeout within the surrounding exit budget.

Unsubscribing is not synonymous with unloading every thread or shutting down a
shared daemon. The implementation logs cleanup errors and timeout outcomes;
the UI leaving does not certify durable completion.

## Edge cases and invariants

- **Review mode** counts as cancellable work. Ctrl+C should interrupt review, not
  quit.
- **Modal open** means Ctrl+C/Ctrl+D should not quit unless the modal explicitly
  declines to handle Ctrl+C.
- **Run in background** deliberately leaves daemon-owned work running.
- **Timeout** bounds UI waiting, not backend resource ownership or durable drain.

## Testing expectations

At a minimum, we want coverage for:

- Ctrl+C while working interrupts, does not quit.
- Ctrl+C while idle and empty requests exit without a double-press timer.
- Ctrl+D with modal open does not quit.
- Ctrl+D while idle and empty requests exit on the first press.
- `/quit` / `/exit` follow disconnect cleanup; `/logout` handles both success and failure.
- Daemon exit choices preserve background work or explicitly interrupt it as selected.
- Cleanup timeout/error paths do not claim durable shutdown acknowledgement.

Use existing app lifecycle and background-exit fixtures. Local compilation and
tests require explicit authorization; otherwise report executable coverage as
pending remote CI.

## History (high level)

Earlier designs used a one-second double-press hint and core
`Op::Shutdown`/`ShutdownComplete` as the exit boundary. PR #8936 records that
history. Those earlier descriptions must not be read as the current
app-server unsubscribe contract.
