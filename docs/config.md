# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## MCP tool approvals

Codex stores per-tool approval overrides for custom MCP servers under
`mcp_servers` in `~/.codex/config.toml`:

```toml
[mcp_servers.docs.tools.search]
approval_mode = "approve"
```

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

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

## Custom CA Certificates

Codex can trust a custom root CA bundle for outbound HTTPS and secure websocket
connections when enterprise proxies or gateways intercept TLS. This applies to
login flows and to Codex's other external connections, including Codex
components that build reqwest clients or secure websocket clients through the
shared `codex-client` CA-loading path and remote MCP connections that use it.

Set `CODEX_CA_CERTIFICATE` to the path of a PEM file containing one or more
certificate blocks to use a Codex-specific CA bundle. If
`CODEX_CA_CERTIFICATE` is unset, Codex falls back to `SSL_CERT_FILE`. If
neither variable is set, Codex uses the system root certificates.

`CODEX_CA_CERTIFICATE` takes precedence over `SSL_CERT_FILE`. Empty values are
treated as unset.

The PEM file may contain multiple certificates. Codex also tolerates OpenSSL
`TRUSTED CERTIFICATE` labels and ignores well-formed `X509 CRL` sections in the
same bundle. If the file is empty, unreadable, or malformed, the affected Codex
HTTP or secure websocket connection reports a user-facing error that points
back to these environment variables.

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

- `auto`: existing adaptive backgrounds (with syntax-theme diff-scope overrides when available).
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

## Realtime start instructions

`experimental_realtime_start_instructions` lets you replace the built-in
developer message Codex inserts when realtime becomes active. It only affects
the realtime start message in prompt history and does not change websocket
backend prompt settings or the realtime end/inactive message.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
