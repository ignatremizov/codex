# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## Shell command timeout

Set a default timeout (in milliseconds) for shell commands when `timeout_ms` is not provided:

```toml
exec_command_timeout_ms = 30000
```

If unset, Codex uses the built-in default (10,000 ms).

## Unified exec yield windows

Set defaults (in milliseconds) for unified exec output capture when `yield_time_ms` is not provided:

```toml
unified_exec_yield_time_ms = 10000 # exec_command initial snapshot window
unified_exec_write_stdin_yield_time_ms = 250 # write_stdin polling window
```

If unset, Codex uses the built-in defaults (10,000 ms initial snapshot window for `exec_command`,
250 ms polling window for `write_stdin`).

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## TUI

Hide the compacted prompt output after `/compact`:

```toml
[tui]
show_compact_summary = false
```

When unset, the transcript includes the compacted prompt when available (otherwise just the summary).

Configure diff add/remove line backgrounds:

```toml
[tui]
# auto (default), off, theme, custom
diff_background = "theme"
```

- `auto`: existing built-in adaptive backgrounds.
- `off`: disable add/remove line backgrounds.
- `theme`: use syntax-theme scope backgrounds (`markup.inserted`/`markup.deleted`, then
  `diff.inserted`/`diff.deleted` fallback).
- `custom`: use explicit colors below (invalid/missing values fall back to `auto` colors):

```toml
[tui]
diff_background = "custom"
diff_add_bg = "#213A2B"
diff_del_bg = "#4A221D"
```

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
