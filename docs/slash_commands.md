# Slash commands

For an overview of Codex CLI slash commands, see [this documentation](https://developers.openai.com/codex/cli/slash-commands).

## Permission settings in this fork

`/approvals` is an alias for `/permissions`. Both open the same permission settings
and accept the same inline profile selection, for example `/approvals :read-only`.
Profile availability, managed requirements, reviewer selection, and full-access
confirmation are resolved through the same server-owned discovery and validation
path; the alias does not bypass approvals or change sandbox permissions itself.
`/approve` remains a separate command for retrying a recent auto-review denial.

## Completed compaction

In the TUI, `/compact` summarizes the conversation history and prints the compacted prompt when available (falling back to the summary) so you can review it. Set `tui.show_compact_summary = false` in `config.toml` to hide the compact output; display-only decoding failures remain visible as diagnostics. Local compaction has a 15-minute response deadline and a half-context-window output cap; remote V2 compaction is governed separately by the server.

During a regular active turn, `/compact` waits for the current sampling/tool batch
to finish, compacts, and continues that turn automatically. Idle `/compact` remains
a standalone operation. Models can request the same operation for their own thread
with `compact {}`. See [model and active user compaction](model-compaction.md).

`/agent` opens the native agent control pane. It also accepts compact commands for spawning, prompting, queueing, interrupting, resuming, closing, and changing one-turn response observation; see [TUI user-controlled multi-agent dispatch](tui-agent-control.md).
