# Slash commands

For an overview of Codex CLI slash commands, see [this documentation](https://developers.openai.com/codex/cli/slash-commands).

In the TUI, `/compact` summarizes the conversation history and prints the compacted prompt when available (falling back to the summary) so you can review it. Set `tui.show_compact_summary = false` in `config.toml` to hide the compact output; display-only decoding failures remain visible as diagnostics. Local compaction has a 15-minute response deadline and a half-context-window output cap; remote V2 compaction is governed separately by the server.

`/agent` opens the native agent control pane. It also accepts compact commands for spawning, prompting, queueing, interrupting, resuming, closing, and changing one-turn response observation; see [TUI user-controlled multi-agent dispatch](tui-agent-control.md).
