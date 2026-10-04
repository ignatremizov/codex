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

In the TUI, `/compact` summarizes the conversation history and prints the compacted prompt when available (falling back to the summary) so you can review it. Set `tui.show_compact_summary = false` in `config.toml` to hide the compact output. Local compaction has a 15-minute response deadline and a half-context-window output cap; remote V2 compaction is governed separately by the server.
