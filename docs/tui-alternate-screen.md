# TUI Alternate Screen and Terminal Multiplexers

## Overview

Codex can render in an owned alternate screen or inline in the terminal. The
current `auto` setting enables the alternate screen, including under Zellij;
it does not detect a multiplexer to choose inline mode. Use `never` or
`--no-alt-screen` when terminal scrollback is required.

## The Problem

### Fullscreen TUI Benefits

Codex's TUI uses the terminal's **alternate screen buffer** to provide a clean fullscreen experience. This approach:

- Uses the entire viewport without polluting the terminal's scrollback history
- Provides a dedicated environment for the chat interface
- Mirrors the behavior of other terminal applications (vim, tmux, etc.)

### Historical Zellij motivation

Earlier Codex designs treated alternate-screen scrollback limitations in Zellij
as a reason to select inline mode automatically. The discussion referenced:

- **Zellij PR:** https://github.com/zellij-org/zellij/pull/1032
- **Rationale:** The xterm spec explicitly states that alternate screen mode disallows scrollback

Terminal scrollback and Codex's own transcript navigation are separate. The
owned viewport can navigate conversation history even when the terminal does
not expose alternate-screen scrollback.

## Current configuration

Three modes are accepted by `tui.alternate_screen` in `config.toml`:

### 1. `auto` (default)

- **Behavior:** Enable alternate screen mode.
- **Multiplexers:** No Zellij-specific exception is applied.

### 2. `always`

- **Behavior:** Always use alternate screen mode (original behavior)
- **Use case:** Users who prefer fullscreen and don't use Zellij, or who have found a workaround

### 3. `never`

- **Behavior:** Never use alternate screen mode (inline mode)
- **Use case:** Users who always want scrollback history preserved
- **Trade-off:** Pollutes the terminal scrollback with TUI output

## Runtime Override

The `--no-alt-screen` CLI flag can override the config setting at runtime:

```bash
codex --no-alt-screen
```

This runs the TUI in inline mode regardless of the configuration, useful for:

- One-off sessions where scrollback is critical
- Debugging terminal-related issues
- Testing alternate screen behavior

## Implementation Details

### Mode selection

`determine_alt_screen_mode()` in `codex-rs/tui/src/lib.rs` applies the CLI
override first. Otherwise it enables the alternate screen unless the setting
is `never`:

```rust
if no_alt_screen {
    return false;
}
tui_alternate_screen != AltScreenMode::Never
```

### Configuration Schema

The `AltScreenMode` enum is defined in `codex-rs/protocol/src/config_types.rs` and serializes to lowercase TOML:

```toml
[tui]
# Options: auto, always, never
alternate_screen = "auto"
```

The old multiplexer-detection snippet is not part of this selection path.
Configuration remains explicit so callers can choose inline rendering without
depending on multiplexer detection.

## Related Issues and References

- **Original Issue:** [GitHub #2558](https://github.com/openai/codex/issues/2558) - "No scrollback in Zellij"
- **Implementation PR:** [GitHub #8555](https://github.com/openai/codex/pull/8555)
- **Zellij PR:** https://github.com/zellij-org/zellij/pull/1032 (why scrollback is disabled)
- **xterm Spec:** Alternate screen buffers should not have scrollback

## Historical alternatives

### Alternative Approaches Considered

Earlier design notes considered custom TUI scrollback, a multiplexer option, and
unconditionally disabling the alternate screen. Those notes predate the owned
transcript viewport and do not describe missing capabilities in the current UI.

## Transcript navigation

The owned alternate-screen viewport supports transcript navigation directly.
The `global.open_transcript` action defaults to Ctrl+T; the runtime keymap can
remap it. Transcript presentation and terminal scrollback are distinct surfaces.

In inline mode, switching threads or agents reconstructs only a recent tail of native terminal scrollback: at most `max(160, terminal height × 5)` rows, including the wrapped earlier-history notice. This one-time switch budget is independent of `tui.terminal_resize_reflow_max_rows`, including when ordinary resize replay is uncapped. It does not remove retained history or restrict the transcript pager, and later ordinary resize replay continues to use the configured limit. The owned alternate-screen viewport does not perform this native-scrollback reconstruction.

## For Developers

When modifying TUI code, remember:

- The `determine_alt_screen_mode()` function encapsulates all the logic
- Configuration is in `config.tui_alternate_screen`
- CLI flag is in `cli.no_alt_screen`
- The resolved mode configures alternate-screen and owned-viewport setup.

If you encounter issues with terminal state after running Codex, you can restore your terminal with:

```bash
reset
```
