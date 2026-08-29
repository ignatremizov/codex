# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Multi-agent V2 message delivery

Select the representation for new V2 task and message payloads with:

```toml
[features.multi_agent_v2]
message_delivery = "encrypted_with_audit"
```

The accepted values are `encrypted` (provider-opaque payload only), `encrypted_with_audit` (encrypted payload plus a separate readable audit copy), and `plaintext` (one readable payload). Omitting the setting selects `encrypted_with_audit`. Readable audit copies and plaintext messages can contain sensitive task details; encryption of the recipient payload does not protect those local copies.

This setting does not itself enable a different multi-agent runtime or change the selected model. Delivery validation and rendering use the resolved thread policy; existing persisted messages retain their recorded representation rather than being converted on resume.

For model-authored V2 `spawn_agent`, `send_message`, and `followup_task` calls, audit mode requires both a nonempty `message` and a nonempty readable `task_message`. The audit copy is not substituted into the recipient's encrypted model input. The other two modes reject `task_message`; plaintext receives the normal attributed context wrapper once. A host-supplied message explicitly marked as plaintext retains that provenance independently of the configured model-authored delivery mode.

The combined `message` and `task_message` payload is limited to 8192 UTF-8 bytes. Invalid or oversized payloads are rejected before target lookup, restoration, or spawning, rather than silently truncated. Generic tool-argument logs redact message payloads; intentionally readable communication audit records remain available.

For children, `list_agents` exposes the latest accepted readable assignment as `last_task_message`, or `null` when unavailable; the root entry uses `"Main thread"`. A rejected send does not replace the child's assignment; an accepted opaque assignment clears it, and a completion result does not overwrite it. This is current registry metadata, not a promise that the field itself survives a process restart.

## Subagent history inheritance

Model-authored spawns start fresh by default: V1 omits or sets `fork_context = false`,
and V2 omits `fork_turns` or uses `"none"`. V2's omitted (or blank) argument uses
`features.multi_agent_v2.default_fork_turns`, which defaults to `"none"`. Its
configured value accepts `"none"`, `"all"`, or a positive integer string such as
`"2"`; surrounding whitespace is trimmed and the words are case-insensitive.
An explicit nonblank `fork_turns` takes precedence.

Inheriting full or bounded parent history requires user authorization:

```toml
[agents]
allow_history_forks = true

[features.multi_agent_v2]
default_fork_turns = "none"
```

`allow_history_forks` defaults to false. A selected role's config file can set
the same `[agents]` key to true or false; omission inherits the parent setting.
The configured `default` role also applies when `agent_type` is omitted.
Authorization is checked after that role is resolved, so a role can explicitly
deny an otherwise globally allowed fork. This role-local exception projects
only `allow_history_forks`, not arbitrary agent settings or provider/permission
overrides. Configured default-role identity is retained for cold reload, and V1
and V2 restores reapply the saved role's authorization without replacing live
runtime permissions or service-tier authority. V1 restore keeps its existing
caller-model precedence; V2 keeps its existing stored-model precedence.

Use V1 `fork_context = true` for full history, or V2 `fork_turns = "all"` or a
positive integer string for full or recent-turn context. Selecting a history
default does not grant authorization. These controls apply to model-authored
spawn tools, not explicit user control-plane forks, and do not select Legacy
versus Paginated storage. Child context drops parent-owned runtime notification
fragments while preserving literal user text and the parent's canonical audit.

## Unified exec yield windows

The optional `unified_exec_yield_time_ms` and `unified_exec_write_stdin_yield_time_ms` settings control the default time before unified-exec returns an output snapshot when the individual tool call does not provide `yield_time_ms`:

```toml
unified_exec_yield_time_ms = 10000
unified_exec_write_stdin_yield_time_ms = 250
```

Omitting either setting, or setting it to zero, uses the built-in default shown above. A per-call `yield_time_ms` takes precedence over the corresponding configured default, including an explicit zero; the existing platform and minimum-yield clamps still apply. These are output-yield windows, not process deadlines, and do not change command execution timeouts.

Initial `exec_command` waits remain bounded to 250–30000 ms, or 10000–30000 ms when Codex runs on Windows. Subsequent `write_stdin` calls have a 5000 ms minimum for empty polls and a 250 ms minimum for non-empty writes. Both can request longer waits without an upper cap by default; process exit can return sooner, and interrupting a poll does not terminate its process.

The optional `background_terminal_max_timeout` setting caps **empty polls only**, in milliseconds:

```toml
background_terminal_max_timeout = 300000
```

Omitting this setting leaves requested empty-poll windows uncapped. A configured value below 5000, including zero, is raised to 5000. It does not cap non-empty writes or change the configured default yield window. In particular, omitting a per-call `yield_time_ms` still uses the default 250 ms shown above, raised to 5000 ms for an empty poll—not an indefinite wait for process exit.

Wait countdowns are advisory estimates, not process deadlines. Pauses or scheduling can extend the actual wait, and unrepresentable deadlines have no countdown. Cancelling a poll does not terminate its process. Turn interruption clears the countdown, but an individually cancelled poll without a subsequent visible lifecycle event can retain its estimate until it expires. History replay does not restart countdowns.

## User-shell command timeout

The optional `user_shell_command_timeout_ms` setting controls the maximum runtime of user-shell commands started with `!` or `/shell`:

```toml
user_shell_command_timeout_ms = 3600000
```

When unset, user shell commands run until they finish, are explicitly stopped through `/stop` or the background-terminal API, or the thread shuts down. Interrupting a model turn does not terminate them. A positive configured value enforces a maximum runtime. Setting the configuration value to `0` leaves the command unbounded; an explicit per-request timeout of `0` instead requests immediate timeout.

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

### Explicit MCP prompt invocation

Set `allow_implicit_invocation = false` on an MCP server to keep its tools out of the default direct tool declarations. The server remains enabled, and permitted tools remain available through deferred discovery, search, and calls. This is a prompt-exposure setting, not an authorization control.

```toml
[mcp_servers.docs]
command = "docs-server"
allow_implicit_invocation = false
```

Use `/mcp use docs` to add that server's complete current tool inventory, including schemas and metadata, to subsequent context in the selected thread. The command preserves the case of the configured server name and trims surrounding whitespace. Existing MCP server-name validation still applies.

Explicit use adds context forward without rewriting earlier messages or promoting tools into the frozen non-Apps direct tool contract. It never starts a model turn by itself. Before the first user turn, the request is queued; during an active turn, insertion waits for a safe input boundary. Each distinct accepted inventory block survives compaction in order. Inventories are deliberately complete and can be large, so invoke only the servers whose details you want in model context.

The default is `true`. Non-Apps direct declarations are frozen from ready inventory when the model first sees tools; Apps remain turn-scoped. Status checks, prewarming, and activation queries do not themselves freeze the direct contract. Configuration reload and runtime permissions remain authoritative for calls regardless of an older visible declaration or context block.

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

The TUI edits an earlier prompt in place by default. To preserve the source conversation and reopen the selected prompt as an editable draft on a new branch instead, enable:

```toml
[features]
fork_prompt_edits = true
```

The branch retains history before the selected turn. Creating it does not submit the draft or automatically continue a goal, and it does not undo filesystem changes. This option also works when editing an existing Legacy session; it does not change that session's stored history mode.

In-place editing also follows the session's actual stored history mode: Paginated sessions use `thread/revert`, and Legacy sessions use guarded `thread/rollback`. Selection is tied to canonical user-message identity; an incomplete or ambiguous old transcript must be refreshed before editing. The draft is not automatically submitted. If a Legacy mutation or its refresh has an uncertain outcome, the TUI preserves the draft and keeps that conversation read-only for the current TUI process, including after switching away and back. Navigation, copying, other conversations, and quitting remain available. Save the draft before quitting, then reopen the conversation in a new Codex process for canonical recovery. The TUI never repeats the mutation automatically.

## Notify

`notify` is deprecated and will be removed in a future release. Existing configurations still work for compatibility, but new automation should use lifecycle hooks instead.

Codex can run a legacy notification command when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key), falling
back to the `CODEX_SQLITE_HOME` environment variable and then `CODEX_HOME`.
The default does not change with the sandbox mode. Managed requirements can
constrain this location; conflicting environment values produce a startup warning.

## Custom CA Certificates

Codex can trust a custom root CA bundle for outbound HTTPS and secure websocket
connections when enterprise proxies or gateways intercept TLS. This applies to
login flows and to Codex's other external connections, including Codex
components that build reqwest clients or secure websocket clients through the
shared `codex-http-client` CA-loading path and remote MCP connections that use it.

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

Local compaction requests have a 15-minute response deadline and cap generated output at half of
the model context window. Local summaries also retain bounded session metadata, including the
session ID, rollout path, user-turn count, and recent-turn coverage. These limits describe local
compaction only; remote V2 compaction has its own server-side behavior.

## Remote compaction handoff

Remote compaction installs its authoritative replacement history before an isolated helper decodes the handoff for display. The decoded text is presentation metadata: it does not replace the installed history, change the main agent's instructions, or become its next input. A decoder failure does not undo successful compaction.

While the helper runs, the live compaction indicator changes to `Decoding` without restarting its timer or adding a transcript entry. A failed decode is recorded on the completed compaction and remains visible on replay, including when `tui.show_compact_summary` hides the compacted content. Cancellation or an intentionally skipped decoder is not reported as a decode failure.

Set the top-level `remote_compaction_handoff_model` to choose the decoder model. When unset, Codex uses `gpt-5.3-codex-spark` if it is present in the available catalog, otherwise the current turn's model. Set `remote_compaction_handoff_fallback_model` to choose a fallback after a decoder failure. Its default is `gpt-5.6-luna` when available and different from the primary model. Setting both options to the same model disables fallback.

Each attempt has its own 15-minute startup and inference deadline. Cleanup can take longer: fallback does not begin until the previous helper's session actor has terminated. Cancellation does not start a fallback. The helper prefers low reasoning when supported, disables reasoning summaries, and has no tool, extension, or environment access. These are decoder settings, not changes to the main agent's model or reasoning policy.

Completed compaction records also expose the skill names in the latest model-visible inventory in the installed history. This is descriptive metadata, not a new skill activation or discovery mechanism. Older records without the inventory decode as an empty list. `tui.show_compact_summary` controls presentation of the decoded text as it does local compacted output.

Remote V2 rollout checkpoints retain the service's reported output-token count as optional `compaction_summary_tokens` metadata. This is the compaction response's usage, not the display decoder's usage or an estimate of the visible text. Older checkpoints, local compaction, and responses without usage leave it absent; it does not affect model context.

## Command output previews

`tui.command_output_preview_lines` limits inline agent/tool command output and `/ps` previews to 30 screen rows by default. `tui.user_shell_output_preview_lines` independently limits user-shell output to 50 rows. Truncated previews retain a head and tail where space permits, with an omission row; command text is not shortened.

Set either value to `0` to display all retained output. These are client-local presentation settings: they do not change command execution, captured output, or the existing bounded live-output storage. The detailed transcript retains all available output and reports any storage-level omissions separately from omitted display rows.

Subagent prompt and response previews are configured independently:

```toml
[tui]
agent_prompt_preview_lines = 50
agent_response_preview_lines = 0
```

Prompt previews default to 50 wrapped display rows; response previews default to unlimited (`0`). The limits apply to wrapped detail rows, including an omission marker, but exclude the preview title and status line. They affect presentation only. Complete prompt and response content remains available in canonical history and full transcript exports; ordinary replay uses the same presentation caps.

## Thread naming

Thread naming is on demand. `/rename` opens an editable name prompt and requests a suggestion from the recent conversation; sending a normal message does not launch title generation. A suggestion is not saved until you confirm the name. Existing names and manual renames remain authoritative, and canceled or stale suggestions cannot overwrite another prompt.

## TUI notification previews

The following `[tui]` settings limit notification previews by Unicode grapheme clusters. The agent-turn limit applies to both desktop and ambient-pet previews; the execution-approval and user-input limits apply to their respective desktop notification categories:

```toml
[tui]
agent_notification_preview_graphemes = 200
exec_approval_notification_preview_graphemes = 30
user_input_notification_preview_graphemes = 30
```

These defaults apply when the settings are omitted. A value of `0` produces an empty preview where a preview is available. The `user-input-requested` notification filter is independent from `plan-mode-prompt` and `async-question`; notification enablement, focus, priority, and coalescing policies are unchanged.

## Diff backgrounds

The `[tui]` `diff_background` setting controls insert/delete line backgrounds: `auto` (the default) uses adaptive palette colors with active syntax-theme scope overrides, `off` disables content backgrounds, `theme` uses the same adaptive/theme-scope behavior, and `custom` uses the configured `diff_add_bg`/`diff_del_bg` colors when each value is valid.

```toml
[tui]
diff_background = "custom"
diff_add_bg = "#213A2B"
diff_del_bg = "#4A221D"
```

Custom colors must be six-digit `#RRGGBB` values. Invalid or omitted colors fall back independently to the adaptive palette. These are client-local presentation settings: they follow TUI preference reloads, do not alter stored diff content, and ANSI-16 terminals continue to use foreground-only styling. Existing light-theme line-number gutter styling is separate; when a line has no content fill, its gutter also stays unfilled.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

## Editable dictation and realtime voice

The fork's optional `features.voice_transcription` enables plain-text dictation into the TUI composer. It is separate from `/voice`, which starts or stops upstream's live realtime WebRTC conversation. Dictation does not submit the resulting text automatically. See [Editable composer dictation](dictation.md) for authentication, shortcuts, and recording behavior.

## Realtime start instructions

`experimental_realtime_start_instructions` supplies a fallback override for the
realtime start instructions included in prompt history. Explicit start instructions
attached to the active realtime conversation take precedence. This config setting
does not change websocket backend prompt settings or the realtime end/inactive
message.

## Quit shortcuts

The built-in double-press quit shortcut is disabled. Ctrl+C first respects active
views and composer cancellation; otherwise it interrupts cancellable work or
requests exit when idle. Ctrl+D requests exit only with an empty composer and no
active modal or popup. Daemon-backed running tasks have additional exit choices.
See [Exit and shutdown flow](exit-confirmation-prompt-design.md) for ownership,
cleanup, and timeout boundaries.

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
