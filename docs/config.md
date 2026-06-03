# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

MCP tools default to serialized calls. To mark every tool exposed by one server
as eligible for parallel tool calls, set `supports_parallel_tool_calls` on that
server:

```toml
[mcp_servers.docs]
command = "docs-server"
supports_parallel_tool_calls = true
```

Only enable parallel calls for MCP servers whose tools are safe to run at the
same time. If tools read and write shared state, files, databases, or external
resources, review those read/write race conditions before enabling this setting.

To keep an MCP server connected but hide its tools from the default model
context until the user explicitly opts in, set
`allow_implicit_invocation = false` on that server:

```toml
[mcp_servers.linear]
url = "https://example.com/mcp"
allow_implicit_invocation = false
```

This only affects whether Codex tells the model about the server by default.
The MCP server still starts normally, and its tools remain available to the
runtime. In the TUI, `/mcp use <server>` adds forward-only context for later
turns so the model can use that server explicitly without rebuilding prior
session context.

## MCP tool approvals

Codex stores approval defaults and per-tool overrides for custom MCP servers
under `mcp_servers` in `~/.codex/config.toml`. Set
`default_tools_approval_mode` on the server to apply a default to every tool,
and use per-tool `approval_mode` entries for exceptions:

```toml
[mcp_servers.docs]
command = "docs-server"
default_tools_approval_mode = "approve"

[mcp_servers.docs.tools.search]
approval_mode = "prompt"
```

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

Codex stores "never show again" choices for tool suggestions in `config.toml`:

```toml
[tool_suggest]
disabled_tools = [
  { type = "plugin", id = "slack@openai-curated" },
  { type = "connector", id = "connector_google_calendar" },
]
```

## Editing earlier prompts

The TUI edits an earlier prompt by rolling the current conversation back in place. To preserve the
source conversation and continue the edit on a new branch instead, enable:

```toml
[features]
fork_prompt_edits = true
```

## Multi-Agent V2

By default, MultiAgentV2 `spawn_agent` starts subagents without copying the
parent thread history when `fork_turns` is omitted. Set
`default_fork_turns` to change that default while still allowing explicit
`fork_turns` tool arguments to override it:

```toml
[features.multi_agent_v2]
default_fork_turns = "none" # "none", "all", or a positive integer string
```

## Notify

`notify` is deprecated and will be removed in a future release. Existing configurations still work for compatibility, but new automation should use lifecycle hooks instead.

Codex can run a legacy notification command when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## Multi-agent message delivery

MultiAgentV2 can preserve provider-opaque encrypted delivery, add a model-authored plaintext audit
record, or use one plaintext message:

```toml
[features.multi_agent_v2]
message_delivery = "plaintext" # encrypted | encrypted_with_audit | plaintext
```

`encrypted_with_audit` is the default. It keeps encrypted delivery while exposing a required
`task_message` audit field on `spawn_agent`, `send_message`, and `followup_task`. `encrypted` omits
the readable audit field. `plaintext` uses one readable `message` field and avoids duplicate model
output. The setting governs newly emitted messages; resumed and forked history retains each
persisted agent message's encrypted or plaintext representation. Config-lock exports include the
resolved setting.

## Remote Compaction Handoff

Set `remote_compaction_handoff_model` to override the model used to decode
remote compaction handoff text for display. When unset, Codex uses
`gpt-5.3-codex-spark` if that model is available in the catalog, otherwise it
falls back to the current turn model.

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

Empty `write_stdin` calls are background terminal polls. By default, their `yield_time_ms` wait is
not capped. To impose a maximum poll window:

```toml
background_terminal_max_timeout = 300000
```

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

Configure main-transcript command output previews:

```toml
[tui]
command_output_preview_lines = 30
user_shell_output_preview_lines = 50
agent_prompt_preview_lines = 50
agent_response_preview_lines = 0
```

Set any value to `0` to show all retained output for that category in the main TUI. Agent prompt
previews apply to rendered rows from subagent spawn/input prompts, and agent response previews apply
to rendered rows from subagent output shown after a multi-agent wait completes.

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

## Commit attribution

Codex can add a [git trailer](https://git-scm.com/docs/git-interpret-trailers) to
generated commit messages so commits make Codex's involvement explicit. This
behavior is gated by the `codex_git_commit` feature flag; the top-level
`commit_attribution` setting is only used when that feature is enabled.

Add the following to `~/.codex/config.toml`:

```toml
commit_attribution = "Codex <noreply@openai.com>"

[features]
codex_git_commit = true
```

When enabled, Codex appends a `Co-authored-by:` trailer using the configured
attribution value. If `commit_attribution` is omitted, Codex uses
`Codex <noreply@openai.com>`. Set `commit_attribution = ""` to disable the
trailer while leaving the feature flag enabled.

## OpenTelemetry Trace Metadata

Codex can add static OpenTelemetry span attributes to exported trace spans and
static W3C tracestate fields to propagated trace context:

```toml
[otel.span_attributes]
"example.trace_attr" = "enabled"

[otel.tracestate.example]
alpha = "one"
beta = "two"
```

Nested `otel.tracestate` tables are encoded as semicolon-separated `key:value`
fields inside the named tracestate member. If propagated trace context already
has the named member, Codex upserts configured fields and preserves other fields
in that member. This config shape does not support setting opaque tracestate
member values. Invalid trace metadata entries are ignored during config load and
reported as startup warnings.
