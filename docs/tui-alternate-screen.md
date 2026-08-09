# TUI Alternate Screen and Scrollback

## Normal conversation output

Codex can render the conversation inline on the terminal's primary screen or in the owned transcript viewport. Inline finalized transcript rows are written to ordinary terminal scrollback, while the owned transcript can use the alternate screen when `tui.fullscreen_transcript` is enabled.

Codex also retains source-backed transcript cells. When the terminal width changes, the resize-reflow path can rebuild previously emitted rows at the new width instead of relying on the terminal to rewrap already-rendered text.

This is separate from the terminal's alternate screen:

- **Inline conversation:** primary screen, terminal-native scrollback, source-backed resize reflow.
- **Owned transcript or temporary full-screen surfaces:** alternate screen when enabled, isolated from terminal scrollback.

Temporary full-screen surfaces include the transcript pager, diff view, full-screen approval views, resume picker, and model-migration prompt. Leaving one of these surfaces restores the conversation viewport: inline sessions return to the primary buffer, while owned-transcript sessions retain their screen.

## `tui.alternate_screen`

The `tui.alternate_screen` setting controls whether the TUI may enter the alternate screen. Temporary surfaces and the owned transcript (when `tui.fullscreen_transcript` is enabled) use that gate.

| Value | Current behavior |
| --- | --- |
| `auto` (default) | Allow requested alternate-screen transitions, including the owned transcript when enabled. |
| `always` | Enable every alternate-screen transition requested by the TUI; it currently has the same gate behavior as `auto`. |
| `never` | Never enter the alternate screen; use the inline conversation mode and render temporary surfaces in the primary buffer. |

Configure it in `config.toml`:

```toml
[tui]
alternate_screen = "auto"
```

The `--no-alt-screen` runtime flag overrides the configured value and `fullscreen_transcript`, forcing inline conversation mode and keeping temporary surfaces in the primary buffer.

## Scrollback and resize reflow

Terminal scrollback capacity is controlled by the terminal emulator. Codex separately limits how many source-backed transcript rows it rebuilds during initial replay and terminal resize.

The `tui.terminal_resize_reflow_max_rows` setting controls the source-backed replay cap for the inline path:

- Omit it to use terminal-specific automatic defaults.
- Set a positive integer to choose an explicit row cap.
- Set it to `0` to disable the Codex row cap and retain all available source-backed rows.

The automatic fallback is 1,000 rows for terminals without a dedicated value, including Ghostty. This cap does not create terminal scrollback or change the terminal emulator's own retention limit.

When switching threads or agents in inline mode, the TUI reconstructs only a recent native-scrollback tail, bounded by `max(160, terminal height × 5)` rows. This switch budget is independent of the resize-reflow cap and does not remove retained history or restrict the transcript pager.

Alternate-screen surfaces do not have standard terminal scrollback. They provide their own navigation over the content they render; for example, the transcript pager opened with Ctrl+T navigates Codex's retained transcript.

## Terminal multiplexers

Multiplexers such as Zellij may strictly disable scrollback while an application is in the alternate screen. That affects the owned transcript and temporary alternate-screen surfaces, not the inline conversation.

Set `tui.alternate_screen = "never"` or pass `--no-alt-screen` when buffer switching itself is undesirable in a terminal or multiplexer.

## Implementation notes

- `tui::init()` creates an inline viewport on the primary screen.
- Startup resolves `TranscriptMode` from `fullscreen_transcript` and alternate-screen permission, then configures screen ownership.
- `determine_alt_screen_mode()` decides whether calls to `Tui::enter_alt_screen()` are enabled.
- `Tui::enter_alt_screen()` and `Tui::leave_alt_screen()` bracket temporary full-screen surfaces.
- `app/resize_reflow.rs` rebuilds normal terminal scrollback from retained transcript cells.
- `resize_reflow_cap.rs` resolves the configured or terminal-specific replay cap.

Related history:

- [GitHub issue #2558](https://github.com/openai/codex/issues/2558)
- [GitHub pull request #8555](https://github.com/openai/codex/pull/8555)
- [Zellij pull request #1032](https://github.com/zellij-org/zellij/pull/1032)

If terminal state is not restored after an abnormal exit, run:

```bash
reset
```
