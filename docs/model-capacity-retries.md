# Model-capacity retries

When a Responses request reports the structured `server_is_overloaded` error,
Codex automatically retries the active turn up to five times. This applies to
ordinary agent turns, including subagents. It does not require a new user
message or a model change.

The retry budget belongs to the turn, not an individual HTTP request or stream.
Sampling and compaction share it, so partial progress, a new sampling step, or
compaction cannot silently replenish the five retries. Internal review sessions
keep their existing deadline-bound retry policy.

Without server retry advice, the waits are 5, 10, 20, 40, and 60 seconds, each
with up to 10% additional jitter. A supplied `Retry-After` deadline takes
precedence when exposed by the transport. HTTP error response headers are
supported; the existing SSE-enclosing and WebSocket nested-header handling is
unchanged. The HTTP request layer passes structured capacity failures directly
to this policy instead of multiplying attempts with its own retries. Rate
limits, quota errors, and unclassified service failures retain their existing
handling.

The TUI displays the retry number and an advisory countdown. Escape interrupts
the wait through the normal turn-cancellation path. A countdown reaching zero
does not submit another turn; the backend remains responsible for retries.
Replayed or stale-turn notifications cannot arm the countdown.

Retries reuse the current turn and its recorded history, including completed
tool results. They do not inject a synthetic “continue” prompt, deliberately
rerun completed tools, or select another model or transport. This uses the
existing response-stream recovery boundary; it is not an exactly-once guarantee
for external tools independently requested by later model output.

If capacity is reported after all five retries have been used, the turn reports
the capacity error normally. With consecutive failures this means six requests:
the initial request plus five retries. Dependent queued prompts and pending
steering inputs remain paused in the TUI for the user to review
instead of silently running against an incomplete answer. A new user turn gets
a fresh retry budget. The budget is in-memory; this feature does not resume
historically failed turns or survive a process restart.

The app-server carries the countdown in nullable `error.retryAt` Unix seconds.
`willRetry` remains authoritative, and older notifications without a deadline
remain valid. See [the app-server notification contract](../codex-rs/app-server/README.md#advisory-wait-countdowns).
