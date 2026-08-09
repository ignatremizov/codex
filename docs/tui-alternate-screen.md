# TUI Alternate Screen and Scrollback

## Normal conversation output

The normal Codex conversation runs in an inline viewport on the terminal's primary screen. Finalized transcript rows are written to ordinary terminal scrollback, so they remain available through the terminal's normal scrolling and selection behavior.

Codex also retains source-backed transcript cells. When the terminal width changes, the resize-reflow path can rebuild previously emitted rows at the new width instead of relying on the terminal to rewrap already-rendered text.

This is separate from the terminal's alternate screen:

- **Inline conversation:** primary screen, terminal-native scrollback, source-backed resize reflow.
- **Temporary full-screen surfaces:** alternate screen when enabled, isolated from terminal scrollback.

Temporary full-screen surfaces include the transcript pager, diff view, full-screen approval views, resume picker, and model-migration prompt. Leaving one of these surfaces restores the saved inline viewport.

## `tui.alternate_screen`

The `tui.alternate_screen` setting controls whether temporary surfaces may enter the alternate screen. It does not move the normal conversation into that buffer.

| Value | Current behavior |
| --- | --- |
| `auto` (default) | Keep the conversation inline and allow temporary full-screen surfaces to use the alternate screen. |
| `always` | Enable every alternate-screen transition requested by the TUI. The normal conversation does not currently request one, so this has the same observable behavior as `auto`. |
| `never` | Never enter the alternate screen. Temporary surfaces render without switching away from the inline terminal buffer. |

Configure it in `config.toml`:

```toml
[tui]
alternate_screen = "auto"
```

The `--no-alt-screen` runtime flag overrides the configured value:

```bash
codex --no-alt-screen
```

The flag disables temporary alternate-screen transitions. It is not required to make normal conversation output use terminal scrollback; normal conversation output is already inline.

## Scrollback and resize reflow

Terminal scrollback capacity is controlled by the terminal emulator. Codex separately limits how many source-backed transcript rows it rebuilds during initial replay and terminal resize.

The `tui.terminal_resize_reflow_max_rows` setting controls that Codex replay cap:

- Omit it to use terminal-specific automatic defaults.
- Set a positive integer to choose an explicit row cap.
- Set it to `0` to disable the Codex row cap and retain all available source-backed rows.

The automatic fallback is 1,000 rows for terminals without a dedicated value, including Ghostty. This cap does not create terminal scrollback or change the terminal emulator's own retention limit.

Alternate-screen surfaces do not have standard terminal scrollback. They provide their own navigation over the content they render; for example, the transcript pager opened with Ctrl+T navigates Codex's retained transcript.

## Terminal multiplexers

Multiplexers such as Zellij may strictly disable scrollback while an application is in the alternate screen. That affects temporary alternate-screen surfaces, not the normal inline conversation.

Set `tui.alternate_screen = "never"` or pass `--no-alt-screen` when buffer switching itself is undesirable in a terminal or multiplexer.

## Implementation notes

- `tui::init()` creates an inline viewport on the primary screen.
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
