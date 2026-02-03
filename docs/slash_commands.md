# Slash commands

For an overview of Codex CLI slash commands, see [this documentation](https://developers.openai.com/codex/cli/slash-commands).

In the TUI, `/compact` summarizes the conversation history and prints the compacted prompt when available (falling back to the summary) so you can review it. Set `tui.show_compact_summary = false` in `config.toml` to hide the compact output.

Compaction turns are capped at 15 minutes and at most 50% of the model's context window for output tokens to avoid runaway compactions.
When compaction runs locally, Codex appends session metadata (session id, rollout path, and large-turn sizes) to the compacted prompt so the model can locate full history later if needed.
