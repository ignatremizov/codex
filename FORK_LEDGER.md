# Codex Fork Ledger

This is the living maintenance map for capabilities carried by `fork` beyond its audited upstream integration base. It is a capability inventory, not a chronological changelog or a requirement to edit documentation in every feature commit.

The 0.160 integration is pinned to upstream source commit `cb7799623b2241f536d799f2d46259148fb987ce`, including its maintenance backports. The upstream release-notes/version commit `a956835d020762cb2b570053af06f643a11c0ecc` is deliberately excluded. Neither rolling `upstream/main` nor local `main` defines the integration base. Keep release-version changes at the final release owner.

The replay manifest inventories the 130 original downstream commits in `f1b21bb2931f86819e32df0cafa1bf69570d84ee..ac0f63abe470b8fa83d7852db7e5d39282c88434`. Reviewed dispositions distinguish retained owners, target adaptations, and explicitly deferred repairs assigned to their defining feature. After integration completes, inspect the resulting downstream stack with `git log --reverse cb7799623b2241f536d799f2d46259148fb987ce..fork`; during replay, `fork` still names the original branch tip. The complete pre-rebase ledger remains the policy-preservation checklist; a partially replayed foundation is not evidence that its later capabilities were dropped or that the new source has passed CI.

Use exact semantic commit subjects as ownership anchors rather than commit hashes. Refresh those anchors after splitting, squashing, or rewording their owning commits. Describe each capability's purpose, implementation entrypoints, and upstream integration seams so its ownership remains understandable after rebasing.

Direct CLI/TUI use, including over a remote terminal or SSH, is the supported client workflow. This fork does not promise app-server wire compatibility with ChatGPT web or mobile clients targeting a different upstream schema.

## Classification

| Kind | Meaning |
| --- | --- |
| Compatibility | Restores or preserves a preferred behavior that upstream removed or changed. |
| Capability | Adds user-visible runtime behavior not present upstream. |
| Observability | Makes otherwise opaque runtime state inspectable or auditable. |
| Efficiency | Reduces model-context, rendering, storage, or interaction overhead. |
| Platform | Maintains support for a platform or transport needed by this fork. |
| Release | Identifies and packages the maintained fork. |

## Maintained Capabilities

This checkpoint inventories the integrated owners through per-spawn model/role settings, on-demand naming, graph-preserving singular deletion, inherited prompt editing, and queued user-shell completion policies, including copied cross-home lineage, scoped replies, target-owned queued turns, and user-controlled delegation. Later replay commits and unimplemented proposals are not represented as completed features. Entrypoints name the current integrated layout; runtime capability is not a claim of executable validation.

| Capability | Kind | Purpose | Primary fork entrypoints | Required upstream seams | Commits |
| --- | --- | --- | --- | --- | --- |
| Canonical model slugs and stable accents | Compatibility | Show canonical active-model slugs in session headers and footers while retaining catalog labels in pickers and historical analytics; preserve stable thread-name and title accents without reverting newer layouts, warning controls, or key notation. | `codex-rs/tui/src/bottom_pane/status_line_style.rs`<br>`codex-rs/tui/src/chatwidget/session_flow.rs`<br>`codex-rs/tui/styles.md` | Current model selection, status/footer rendering, session startup, TUI snapshots | `revert(tui): restore canonical model slugs and stable accents` |
| Private Guardian screenshot evidence | Compatibility | Preserve missing private-review screenshot evidence with original provenance after compaction without replaying already admitted text or weakening reviewer input budgets and source isolation. | `codex-rs/core/src/context/node_repl_review_evidence.rs`<br>`codex-rs/core/src/guardian/input_budget.rs`<br>`codex-rs/core/src/guardian/review_session_images.rs` | Private review lifecycle, current screenshot selection, compaction, input budgeting | `fix(guardian): preserve private screenshot evidence across compaction` |
| Compact `apply_patch` tool guidance | Efficiency | Keep the model-visible patch contract explicit while avoiding redundant per-session instruction tokens. | `codex-rs/core/src/tools/handlers/apply_patch_spec.rs` | Tool schema construction and schema tests | `fix(tools): shorten the apply_patch freeform description` |
| Complete repeated `apply_patch` history | Observability | Preserve every verified update hunk when one patch edits the same file repeatedly so inline summaries, transcript review, and persisted completion events do not show only the final section. | `codex-rs/apply-patch/src/invocation.rs`<br>`codex-rs/tui/src/diff_render.rs` | Apply-patch verification, structured change aggregation, TUI diff rendering | `fix(apply-patch): verify repeated updates as one canonical file change` |
| Unlabelled compact rate-limit percentages | Compatibility | Preserve the compact rate-limit presentation instead of adding repeated window labels to status surfaces. | `codex-rs/tui/src/chatwidget/status_controls.rs`<br>`codex-rs/tui/src/bottom_pane/status_line_setup.rs` | TUI status controls and snapshots | `fix(tui): shorten compact rate-limit percentage labels` |
| `/approvals` compatibility command | Compatibility | Retain the familiar permissions command alongside `/permissions` without conflicting with the separate `/approve` auto-review action. | `codex-rs/tui/src/slash_command.rs`<br>`codex-rs/tui/src/chatwidget/slash_dispatch.rs` | TUI slash-command catalog and dispatch | `fix(tui): restore /approvals as an alias for permission settings` |
| In-place prompt editing by default | Compatibility | Roll a thread back when editing an earlier prompt, preserving the exact selected durable boundary across compaction, SQLite projection rebuilds, resume, and inherited fork prompts while leaving source-preserving prompt forks available through `fork_prompt_edits`. Duplicate rendered copies use turn/item identity; the latest session header and rejected edit draft are retained. | `codex-rs/tui/src/app_backtrack/prompt_projection.rs`<br>`codex-rs/tui/src/app/legacy_prompt_edit.rs`<br>`codex-rs/tui/src/app/event_dispatch.rs`<br>`codex-rs/core/src/session/handlers.rs`<br>`codex-rs/protocol/src/protocol.rs`<br>`codex-rs/thread-store/src/local/thread_history_materialization.rs` | App-server rollback, durable rollout markers, paginated thread-history projection, TUI thread/event lifecycle, feature configuration | `feat(tui): add opt-in source-preserving prompt edits`, `fix(rollback): preserve exact durable Legacy thread boundaries`, `fix(tui): restore prompt editing across forked histories` |
| Deferred tool discovery in Lite and Code Mode | Capability | Keep `tool_search` reachable when V1 tools are deferred by Responses Lite and expose ranked deferred MCP discovery inside Code Mode without injecting every schema up front. | `codex-rs/core/src/client.rs`<br>`codex-rs/core/src/tools/handlers/tool_search.rs`<br>`codex-rs/tools/src/code_mode.rs` | Responses request serialization, tool routing, Code Mode declarations/runtime | `fix(client): keep Lite tool search reachable for deferred V1 agents`, `fix(code-mode): expose ranked deferred tool discovery` |
| Non-persistent raw response traces | Efficiency | Preserve opt-in transport diagnostics while excluding full Responses API payloads from the SQLite log sink to reduce sensitive-data retention and insert/prune churn. | `codex-rs/codex-api/src/sse/responses.rs`<br>`codex-rs/codex-api/src/endpoint/responses_websocket.rs` | State log filtering | `fix(logging): exclude raw response events from SQLite and feedback` |
| Complete role and model selection for subagents | Capability | Expose built-in roles without requiring custom role configuration, allow MultiAgent V2 to spawn any loaded catalog model regardless of its default multi-agent tag, keep collaboration tools available to those V2 children, keep explicit model and reasoning overrides independent of full-history inheritance in both agent tool versions, and let configured roles replace inherited base instructions from a role-local non-empty file while preserving separate developer instructions and custom provenance. | `codex-rs/core/src/agent/role.rs`<br>`codex-rs/core/src/agent/child_config.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`<br>`codex-rs/core/src/tools/spec_plan.rs` | Spawn tool schemas, model catalog validation, child tool exposure, history inheritance, role-local instruction loading, TUI app-server session configuration | `fix(multi-agent): expose built-in roles and retain authoritative runtime schemas`, `fix(multi-agent): resolve child model overrides across catalog runtime tags`, `feat(multi-agent): require user authorization for inherited history`, `feat(multi-agent): add per-spawn model and reasoning overrides` |
| Interactive and resumable subagent threads | Compatibility | Keep parent-owned and V2 subagent threads directly inspectable and interactive, revive replayed or cold-resumed children through their owning control plane, and preserve routing, identity, completion delivery, graph state, and parent-return behavior. | `codex-rs/core/src/agent/control/spawn.rs`<br>`codex-rs/core/src/thread_manager/v2_spawn_resume.rs`<br>`codex-rs/tui/src/app/thread_resume.rs`<br>`codex-rs/tui/src/app/agent_navigation.rs` | App-server direct input, agent ownership and registry state, V1 adoption, V2 graph restoration, TUI thread navigation | `fix(tui): allow direct child-thread input while preserving writer quarantine`, `fix(app-server): allow direct child input and caller-controlled cold resume`, `feat(multi-agent): restore recorded agents through their live owning control` |
| Configurable and auditable V2 agent messaging | Observability | Select encrypted, encrypted-with-audit, or plaintext delivery; retain complete readable assignments where permitted; and preserve readable communication in rollout traces without confusing audit text with transport correlation. | `codex-rs/core/src/agent_communication.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents_v2/`<br>`codex-rs/rollout-trace/src/reducer/tool/agents.rs` | Thread config locking, agent registry/control, spawn/send/follow-up schemas, rollout reduction | `feat(config): select the multi-agent V2 message delivery policy`, `feat(multi-agent): separate readable audit from V2 message delivery`, `feat(rollout-trace): retain readable content on agent interaction edges` |
| Configurable context inheritance for V2 spawns | Efficiency | Default new V2 subagents to no parent turns while allowing `none`, `all`, or a bounded turn count through `features.multi_agent_v2.default_fork_turns`; inherited history additionally requires the user's `agents.allow_history_forks` authorization. Explicit history forks keep parent audit records canonical while excluding stale parent-agent delivery envelopes from child instructions. | `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`<br>`codex-rs/core/src/agent/control/spawn.rs` | Feature configuration, spawn schema and parsing, fork-history filtering | `feat(multi-agent): require user authorization for inherited history` |
| Auditable multi-agent transcripts | Observability | Show useful multi-line task prompts and completed wait responses with independent configurable caps, preserve readable V2 task messages in parent and child histories, surface attributable inter-agent communication through live and resumed transcripts without exposing encrypted content, and resolve collab labels from thread-wide canonical nickname/role metadata across paginated pages and complete exports. | `codex-rs/tui/src/multi_agents.rs`<br>`codex-rs/tui/src/thread_transcript.rs`<br>`codex-rs/tui/src/app/history_pagination.rs`<br>`codex-rs/tui/src/app/thread_routing.rs`<br>`codex-rs/tui/src/chatwidget/tool_lifecycle.rs`<br>`codex-rs/app-server-protocol/src/protocol/v2/item.rs`<br>`codex-rs/app-server/src/request_processors/response_item_transcript.rs`<br>`codex-rs/thread-store/src/local/thread_history_materialization.rs` | Canonical collab item conversion, TUI tool-history rendering and receiver cache, app-server thread history and notifications, durable rollout reconstruction, fork-owned history projections | `feat(tui): configure source-preserving agent prompt and response previews`, `feat(multi-agent): preserve readable task prompts in parent activity`, `feat(multi-agent): publish durable inter-agent transcripts with typed provenance` |
| Durable background subagent completions | Observability | Show terminal v1 and v2 child results immediately when no active wait owns presentation, retain their full model-visible context and canonical transcript rows through cancellation, rollback, shutdown, compaction, pagination, and cold resume, and allow a later explicit wait to render independently. | `codex-rs/core/src/session/sub_agent_completion.rs`<br>`codex-rs/core/src/agent/control/presentation.rs`<br>`codex-rs/protocol/src/sub_agent_completion.rs`<br>`codex-rs/thread-store/src/completion_artifacts.rs`<br>`codex-rs/tui/src/multi_agents/background_completion.rs` | Agent status publication, parent Session lifecycle, rollout persistence and reconstruction, app-server history projection, TUI collab rendering | `feat(multi-agent): persist and surface exact-instance background completions` |
| Event-driven subagent response observation | Capability | Let V1 lifecycle calls request the first complete commentary, subscribe to an exact target turn's final reply, or explicitly avoid a new final subscription through compact `w` flags, while coordinating wait presentation and independent exact-instance observers. Delivered audit survives rollback, compaction, recovery, and migration; cold history never recreates pending subscriptions. | `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`<br>`codex-rs/core/src/agent/control/response_observer.rs`<br>`codex-rs/core/src/agent/control/response_delivery.rs`<br>`codex-rs/core/src/session/response_observation.rs`<br>`codex-rs/thread-store/src/local/rollout_migration/canonicalizer.rs`<br>`docs/multi-agent-v1-response-observation.md` | Agent lifecycle tool schemas, exact-turn admission, durable rollout context, completion publication, rollout migration, TUI commentary/final presentation | `feat(multi-agent): bind response observation to exact admitted turns` |
| Native user agent control plane | Capability | Let users inspect, spawn, prompt, queue, interrupt, resume or explicitly adopt by UUID, close, and change one-turn response observation for V1 and V2 agents through `/agent`, using durable root-scoped refs and nicknames while preserving canonical UUID ownership, structured user input, exact-turn delivery, source-side audit history, transcript inspection, and explicit fork modes. Unknown input outcomes require manual reconciliation, never automatic retry. | `codex-rs/core/src/agent/user_control/`<br>`codex-rs/state/src/runtime/agent_aliases/`<br>`codex-rs/app-server/src/request_processors/thread_processor/agent_control.rs`<br>`codex-rs/tui/src/chatwidget/agent_command.rs`<br>`codex-rs/tui/src/app/agent_control_pane.rs`<br>`docs/tui-agent-control.md` | Agent graph persistence and transfer, app-server v2 control APIs, response observation, fork-history projection, TUI composer and thread routing | `feat(multi-agent): add user-controlled agent dispatch and durable aliases`, `feat(multi-agent): add scoped replies and target-owned queued turns` |
| Scoped replies and target-owned queued turns | Capability | Bind optional V1 reply permission to exact live endpoints and admitted target work; share a process-lifetime FIFO between model and user queued input; keep queue acceptance distinct from turn admission, prompt persistence, and response delivery. Explicit close replay reuses acknowledged completion provenance rather than inventing a new receipt. | `codex-rs/core/src/agent/control/scoped_messages.rs`<br>`codex-rs/core/src/agent/turn_queue.rs`<br>`codex-rs/core/src/session/observed_input.rs`<br>`codex-rs/core/src/session/close_response.rs`<br>`codex-rs/tui/src/app/agent_prompt_queue.rs` | Shared controller/native runtime separation, input origins, canonical source publication, startup and interruption, completion receipt scope, residency, host-aware queue APIs, reconnect recovery | `feat(multi-agent): add scoped replies and target-owned queued turns` |
| Visible completed compaction | Observability | Expose installed compaction output in TUI, exec, app-server, and JSONL history while preserving current compaction ownership and replacement-history semantics. | `codex-rs/core/src/compact.rs`<br>`codex-rs/core/src/tasks/compact.rs`<br>`codex-rs/exec/src/exec_events.rs` | Core session/turn lifecycle, protocol items, app-server thread history, TUI rendering | `feat(compact): expose completed compaction output across history and clients` |
| User-controlled context automation | Compatibility | Keep token budgeting, private context management, and automatic TUI recaps disabled until the user explicitly enables them; prevent model-catalog metadata from activating hidden work; retain manual `/recap`; and keep the visible `update_plan` tool available unless explicitly disabled. | `codex-rs/core/src/session/token_budget.rs`<br>`codex-rs/core/src/config/mod.rs`<br>`codex-rs/tui/src/app/recap.rs` | Model-catalog defaults, feature/config resolution, world-state instruction filtering, TUI recap scheduling | `fix(context): preserve user-controlled automation defaults` |
| Inspectable remote compaction handoff | Observability | Decode the provider-opaque post-compaction handoff through a locked-down helper, show a live decode phase, and persist server-reported summary token counts without changing replacement history; the user-controlled remote-compaction feature gate remains authoritative even for provider-capable models. | `codex-rs/core/src/compact_handoff_summary.rs`<br>`codex-rs/core/src/compact_remote_v2.rs`<br>`codex-rs/core/src/compact_remote_history.rs`<br>`codex-rs/core/src/tasks/compact.rs` | Agent delegation, compaction protocol/events, rollout reconstruction, app-server notifications, TUI status | `feat(compact): decode installed remote handoffs for display`, `feat(compact): expose live decoding progress and durable diagnostics`, `feat(history): persist remote compaction output token counts` |
| Compacted-media retention and vacuuming | Efficiency | Remove obsolete inline images and structured tool-output media from compacted model history while retaining bounded, reopenable local-image provenance for one compaction window and providing guarded canonical repair and vacuum paths for existing rollouts. | `codex-rs/core/src/context/compacted_media.rs`<br>`codex-rs/core/src/session/compacted_media_repair.rs`<br>`codex-rs/rollout/src/media_vacuum.rs` | Compaction replacement history, rollout reconstruction and rollback, thread-history projections, plain and compressed rollout storage | `fix(compaction): sanitize retained media and publish canonical history repairs safely` |
| Explicit MCP prompt invocation policy | Capability | Keep configured MCP servers live, deferred, searchable, and callable while hiding selected inventories from direct context until `/mcp use` or `thread/mcpServer/activate` explicitly contributes them. | `codex-rs/core/src/context/mcp_server_use_instructions.rs`<br>`codex-rs/core/src/mcp_tool_exposure.rs`<br>`codex-rs/app-server/src/request_processors/mcp_processor.rs` | MCP catalog/runtime, config editing, session input ordering, compaction, TUI commands, app-server protocol and SDK | `feat(mcp): separate implicit exposure from explicit prompt activation` |
| Cross-home and path-based session forks | Capability | Fork saved or archived history into the active home, copying complete external paginated lineage read-only while preserving ordinary coordinated in-home forks. External UUIDs cannot borrow local runtime or goal authority; flattening cannot manufacture canonical observation evidence. | `codex-rs/cli/src/main.rs`<br>`codex-rs/app-server/src/request_processors/thread_processor.rs`<br>`codex-rs/core/src/thread_manager/external_fork.rs`<br>`codex-rs/thread-store/src/local/fork_copy.rs` | TUI source lookup, app-server `thread/fork`, opened lineage snapshots, rollback/provenance coordinates, runtime/source namespace separation | `feat(cli): fork saved sessions from explicit local sources`, `fix(fork): copy paginated lineage across Codex homes` |
| Durable promoted skills and goal context | Capability | Keep explicitly selected skills discoverable across compaction and resume through canonical receipt publication, reconstruct only current active-goal context, anchor steering to sources authoritative for the objective, prevent ordinary forks/subagents from inheriting goal authority, and tolerate transient user-side Git breakage in configured skill directories. | `codex-rs/ext/goal/src/runtime/objective_projection.rs`<br>`codex-rs/ext/goal/templates/goals/`<br>`codex-rs/ext/skills/src/selection.rs`<br>`codex-rs/core/src/session/durable_context.rs`<br>`codex-rs/core/src/session/checkpoint_publication.rs` | Skills inventory/provider lifecycle, compaction installation, app-server goal/fork APIs, state goal storage, extension contribution ordering | `feat(context): durably publish goal authority and promoted skill inventories`, `fix(goal): preserve objective source authority through steering` |
| Compact discovered skill paths | Efficiency | Preserve canonical skill identity internally while rendering user-facing discovery routes such as `~/.agents/skills/...` instead of resolved checkout paths and repeated home-directory prefixes. | `codex-rs/ext/skills/src/loader/host.rs`<br>`codex-rs/ext/skills/src/provider/host.rs` | Host skill discovery and model-visible inventory rendering | `fix(skills): render compact host discovery paths without changing authority` |
| Named early skill-read activity | Observability | Attribute `SKILL.md` reads to the selected skill even when the TUI receives the tool call before asynchronous skill metadata has populated ChatWidget state. | `codex-rs/tui/src/chatwidget/skills.rs` | TUI tool-call classification and transcript rendering | `fix(tui): retain skill names in read history before discovery completes` |
| ChatGPT OAuth dictation | Capability | Record editable composer dictation with browser authentication, bounded silence-aware chunking, generation-owned cancellation, and ordered transcript insertion through a pinned route-aware HTTP pool. Preparation and individual uploads have deadlines; there is no claimed whole-recording pipeline deadline. | `codex-rs/tui/src/chatwidget/dictation.rs`<br>`codex-rs/tui/src/dictation/chunk_policy.rs`<br>`codex-rs/tui/src/dictation/transcription.rs` | ChatGPT authentication, shared HTTP routing and Cloudflare-cookie policy, configurable keymaps, composer lifecycle, audio dependencies | `feat(tui): add bounded browser-authenticated editable dictation` |
| Native audio transcript markers | Observability | Keep upstream native model-audio inputs distinct from editable OAuth dictation and render an explicit `[audio]` marker for every attachment in live, paginated, resumed, and exported TUI transcripts instead of silently omitting it. | `codex-rs/tui/src/chatwidget/user_messages.rs`<br>`codex-rs/tui/src/thread_transcript.rs` | App-server user-message projection, pending-steer matching, transcript hydration and export | `fix(tui): retain native audio attachments in transcripts` |
| Configurable shell and unified-exec timing | Capability | Configure initial/background yield windows, requested empty-poll caps, and user-shell command deadlines. An omitted background-poll cap allows long requested waits; output remains bounded and cancellation remains authoritative. `!`/`/shell` commands run as detached background work until exit or explicit cancellation by default, with live IDs exposed through `/ps`, completed through `/stop`, and individually stoppable through `/stop <id>` without blocking model turns or queued follow-ups. | `codex-rs/core/src/tools/handlers/unified_exec/`<br>`codex-rs/core/src/tools/handlers/shell_spec.rs`<br>`codex-rs/core/src/tasks/user_shell.rs`<br>`codex-rs/tui/src/history_cell/exec.rs`<br>`codex-rs/tui/src/chatwidget/slash_dispatch.rs` | Config loading/locking, turn context, tool schemas, process manager, app-server background-terminal control, TUI command lifecycle | `feat(config): make unified exec yield defaults configurable per turn`, `feat(unified-exec): allow uncapped requested background poll windows`, `feat(user-shell): configure command deadlines and preserve timeout output`, `feat(user-shell): add explicit long-running process control` |
| User-shell completion policies | Capability | Let user-authored `!` commands choose passive, completion-wake, or presentation-only result delivery through compact `w` flags; order selected commands behind every earlier user-shell submission without blocking ordinary turns; keep queued commands visible and cancellable through `/ps` and `/stop`; suppress goal idle work while a requested completion wake remains pending; and attribute resulting automatic wake turns distinctly from user-authored work. | `codex-rs/core/src/tasks/user_shell.rs`<br>`codex-rs/core/src/session/user_shell_delivery.rs`<br>`codex-rs/core/src/tasks/user_shell_registration.rs`<br>`codex-rs/core/src/unified_exec/user_shell_queue.rs`<br>`codex-rs/protocol/src/protocol.rs`<br>`codex-rs/app-server-protocol/src/protocol/v2/thread.rs`<br>`codex-rs/tui/src/user_shell_command.rs` | User-shell process lifecycle, active-turn steering and idle wake scheduling, Responses turn-trigger metadata, app-server v2 schema, TUI composer parsing and highlighting | `feat(user-shell): add queued completion policies` |
| Fail-closed human command approval deadlines | Capability | Let unattended on-request command approvals expire against an optional core-authoritative monotonic deadline, reject late responses without executing the command, preserve untimed handling when either app-server timing field is absent, and keep Guardian, patch approvals, and omitted deadlines on their existing timing behavior. | `codex-rs/core/src/session/mod.rs`<br>`codex-rs/core/src/tools/approvals.rs`<br>`codex-rs/app-server-protocol/src/protocol/v2/item.rs`<br>`codex-rs/app-server/src/bespoke_event_handling.rs`<br>`codex-rs/tui/src/bottom_pane/approval_overlay.rs` | Approval protocol and callback routing, delegated turn configuration, app-server/MCP response arbitration, TUI pending-request lifecycle | `feat(approvals): enforce fail-closed human command deadlines` |
| Bounded terminal execution, waits, and configurable previews | Observability | Show which terminal and command an empty poll checked, classify ordinary Python commands as executed rather than directory listings, retain bounded output across initial-yield and process-exit races, with explicit omission and output-close deadline limits, recover offset-proven remote replay gaps, render live wait countdowns, and independently cap agent/tool versus user-shell previews. | `codex-rs/core/src/unified_exec/async_watcher.rs`<br>`codex-rs/core/src/unified_exec/head_tail_buffer.rs`<br>`codex-rs/exec-server/src/client_recovery.rs`<br>`codex-rs/shell-command/src/parse_command.rs`<br>`codex-rs/tui/src/chatwidget/command_lifecycle.rs` | Unified-exec process/output ownership, exec-server replay offsets, completed-process cache, app-server terminal notifications, TUI status/history/pager | `feat(tui): configure source-preserving command output previews`, `feat(tui): show lifecycle-owned advisory wait countdowns`, `fix(history): reconstruct Legacy terminal output without restoring cold runtime state`, `fix(tui): classify non-enumerating Python commands as executed`, `fix(unified-exec): retain bounded output across polling and terminal replay` |
| Configurable diff backgrounds | Capability | Select adaptive, disabled, theme-derived, or custom diff backgrounds while preserving syntax highlighting and normalizing tabs before wrapping. | `codex-rs/tui/src/diff_render.rs`<br>`codex-rs/tui/src/render/highlight.rs` | TUI config/schema and startup theme resolution | `feat(tui): configure diff backgrounds through client-local preferences` |
| Configurable desktop notification previews | Capability | Tune agent-turn, exec-approval, and user-input notification lengths instead of relying on hard-coded grapheme limits. | `codex-rs/tui/src/chatwidget/notifications.rs` | TUI config/schema and notification creation/display paths | `feat(tui): configure notification previews with independent grapheme limits` |
| Scalable transcript switching, review, and paging | Efficiency | Bound terminal scrollback replay when switching agents, render transcript pages (Ctrl+T by default, remappable) in proportion to the visible viewport, and default the pager to a concise chronological review with exact retained output one keypress away. | `codex-rs/tui/src/transcript_reflow.rs`<br>`codex-rs/tui/src/pager_overlay.rs`<br>`codex-rs/tui/src/pager_overlay/transcript.rs`<br>`codex-rs/tui/src/render/renderable.rs` | TUI thread switching, resize/reflow, history-cell representation and navigation, hyperlink mapping | `fix(tui): bound thread-switch scrollback replay with a one-shot row budget`, `feat(tui): add logical viewports and cached rows to static pagers`, `feat(tui): share concise transcript review and canonical target navigation` |
| Complete tool-review history | Observability | Retain full code-mode invocation/output and approved Guardian assessments through live and replay presentation without removing current footer lifecycle or raw-history behavior. | `codex-rs/tui/src/history_cell/`<br>`codex-rs/tui/src/chatwidget/tool_requests.rs` | Code-mode history, Guardian decisions, replay, owned viewport | `revert(tui): keep complete code mode calls in history`, `revert(tui): keep approved Guardian assessments visible` |
| TLS and Responses startup failure bounds | Platform | Classify nested TLS trust failures without retrying them and bound pre-header Responses startup separately from post-header stream idle handling. | `codex-rs/http-client/`<br>`codex-rs/codex-api/src/endpoint/responses.rs` | Route-aware HTTP fallback, certificate classification, provider idle timeout | `fix(http-client): classify nested TLS failures without retrying trust errors`, `fix(client): bound HTTP response stream startup` |
| Explicit skill catalog budgets | Efficiency | Honor the configured catalog budget without an extra hidden token cap while preserving executor resource authority. | `codex-rs/ext/skills/` | Provider inventory and model-visible skill descriptions | `fix(skills): honor explicit catalog budgets without a hidden token cap` |
| Canonical legacy history migration and shared evidence | Compatibility | Preserve canonical audit history during migration and prevent thread deletion from destroying shared message-board evidence. | `codex-rs/thread-store/src/local/rollout_migration/`<br>`codex-rs/agent-graph-store/` | Exact rollout lineage, migration and deletion ownership | `fix(history): preserve canonical audit replay during legacy migration`, `fix(message-board): preserve shared evidence during thread deletion` |
| On-demand thread naming | Compatibility | Generate an editable suggestion only when the user requests naming or renaming, preserve the originating thread/request and cancellation scope, and persist only a confirmed name. Ordinary prompts no longer trigger hidden naming work. | `codex-rs/tui/src/app/thread_title.rs`<br>`codex-rs/tui/src/app/event_dispatch.rs`<br>`codex-rs/tui/src/chatwidget/interaction.rs` | Structured temporary requests, thread switching, cancellation, text-suggestion identity, manual name persistence | `fix(tui): make thread naming on demand` |
| Graph-preserving singular deletion | Compatibility | Delete only the selected persisted thread, retaining related sessions, incident spawn edges, and durable alias evidence without reviving a tombstoned identity or expanding survivor resume/transfer authority. Explicit bulk deletion remains strict. | `codex-rs/app-server/src/request_processors/thread_delete.rs`<br>`codex-rs/thread-store/src/local/delete_thread.rs`<br>`codex-rs/thread-store/src/in_memory_deletion.rs`<br>`codex-rs/state/src/runtime/thread_deletion.rs` | Selected-writer and lifecycle locks, Paginated reference veto, message-board protection, tombstone transaction, current removal ownership, TUI/CLI confirmation | `fix(delete): preserve related agent threads` |
| Manual verification and release infrastructure | Release | Keep routine verification separate from release packaging while retaining distinct cache namespaces, macOS link/checkpoint recovery, credential-path smoke coverage, and artifact integrity. The routine verification sccache limit is 4G; other jobs retain their own limits. Workflow presence does not assert a passing run for this checkpoint. | `.github/workflows/manual-verify.yml`<br>`.github/workflows/manual-release-build.yml` | Remote build/test/schema jobs, cache identity, release checkpointing, credential isolation, and packaging | `ci(fork): consolidate manual verification and release infrastructure` |

## Integration-specific ownership details

### Built-in collaboration role schemas

The collaboration tool plan exposes `agent_type` whenever built-in or configured roles are
resolvable. V2 collaboration tools retain their fork-owned bundled parameter schemas, encrypted
argument markers, namespaces, and runtime forwarding even when model catalogs provide incompatible
parameter overrides; catalog descriptions remain independently overridable where supported.
Entrypoints are `core/src/agent/role.rs`, `core/src/tools/spec_plan.rs`, and
`core/src/tools/multi_agent_tool.rs`. Focused schema and request-level coverage lives in the
corresponding core tool tests and scenario fixtures.
Ownership anchor: `fix(multi-agent): expose built-in roles and retain authoritative runtime schemas`.
Upstream's independent direct-message disablement and wait-tool gates remain authoritative.

Explicit child model overrides resolve against the loaded catalog regardless of the selected
multi-agent runtime tag; only the concise picker description is limited to five visible models.
Unknown models remain rejected with the complete loaded catalog listed, while child runtime exposure
continues to follow the resolved turn version.
Ownership anchor: `fix(multi-agent): resolve child model overrides across catalog runtime tags`.
The selection/validation owner is `codex-rs/core/src/agent/child_config.rs`; resolved runtime exposure
stays in `codex-rs/core/src/tools/spec_plan.rs`, independently of the description's display cap.

### Readable audit alongside V2 delivery

Ownership anchors: `feat(config): select the multi-agent V2 message delivery policy` and
`feat(multi-agent): separate readable audit from V2 message delivery`.
`core/src/tools/handlers/multi_agents_v2/message_tool.rs` validates configured opaque/audit/plaintext
payloads before target restoration; `core/src/agent/types.rs` preserves their typed representation.
The local runtime's registry and captured-thread submission path serialize accepted assignments
without retargeting replacement runtimes. Readable assignment metadata is volatile and separate
from canonical encrypted recipient input, human authorization, and durable mailbox ownership.
Runtime-aware tool-log redaction remains independent of upstream's name-only stream diagnostics.
Current direct-message disablement, capacity, root-target, residency, and queue-only/no-wake gates
are preserved. Executable and generated-schema qualification is still pending for this rebase.

### Per-spawn settings and role instructions

Ownership anchor: `feat(multi-agent): add per-spawn model and reasoning overrides`.
The centralized `core/src/agent/child_config.rs` resolves captured parent settings,
configured child defaults, role defaults, and explicit overrides in that order. A model
selection without a same-layer effort chooses the final model default. Explicit effort
and tier validation follow final model resolution; repeating an already configured model
outside discovery remains valid. Model-authored overrides still require explicit user,
AGENTS, or skill instructions. Host-authored spawn origin is distinct from model history
authorization; the public shared controller and captured native owner remain authoritative.

Role-local base instruction files are host configuration, read before applying changes,
and remain custom base provenance rather than developer instructions. Sampling projects
only classified developer configuration for ordinary owned children, preserving canonical
history, managed policy, client text, and private-helper isolation. The same owner now
registers the role-file lifecycle/failure scenarios and sampling call that the historical
stack introduced prematurely as files and activated only in later commits.

Saved child model, effort, and service tier survive controlled cold restoration without a
root-tier overlay. Current shared-controller tier bookkeeping remains available to its
existing callers. Idle child settings checkpoint through the canonical publisher. The
released migration10050 is unchanged; both metadata INSERTs retain all41 current columns,
placeholders, and bindings, including newer target metadata. Clearable tier patches and
Legacy/Paginated metadata views remain synchronized.

The TUI passes explicit model/effort fields through structured user control and canonical
audit, retains quoted selectors and attached-input spans, and refreshes model/effort
completion candidates only from a current catalog response. No generated-contract,
executable, or snapshot qualification is implied by these source changes.

### Authorized parent-history inheritance

Ownership anchor: `feat(multi-agent): require user authorization for inherited history`.
Model-authored V1/V2 spawns authorize full or bounded history only after resolving the selected
role. The global default is false, while a user-authored role may allow or deny that single
setting. V2's configured default selects history, not permission. Entrypoints are
`core/src/agent/child_config.rs`, `core/src/agent/role.rs`, and the two spawn-tool handlers.
Cold restores reapply recorded role policy without replacing the current runtime's permissions,
provider, or service-tier authority. Fork notification filtering changes only child context,
preserving canonical parent history and marking partially retained evidence incomplete.

### Durable inter-agent transcript provenance

Ownership anchor: `feat(multi-agent): publish durable inter-agent transcripts with typed provenance`.
The current detached history-publication worker owns canonical writer flush, prepared source/MCP
metadata, installation, and raw event delivery. World-state and confirmed-message publication
use that same ordering, while settings replies keep their independent pre-event acknowledgement.
`app-server-protocol/src/protocol/v2/inter_agent_message.rs` projects presentation provenance,
not human authorization or a new local assistant answer. Receiving-side ciphertext remains opaque.
Running resume retains a generation-aware per-connection snapshot/delivery boundary; cancelled
resumes require canonical retry. Paginated readers lazily rebuild fork-owned derived transcript
state under retained lineage reservations, without changing upstream migration versions or audit
bytes. TUI, CLI, exports, structured replies, and voice consumers distinguish these transcripts
from final assistant output. Generated-schema and executable qualification remain outstanding.

### Exact Legacy rollback and prompt identity

Ownership anchor: `fix(rollback): preserve exact durable Legacy thread boundaries`.
The Legacy `thread/rollback` compatibility route uses guarded canonical decoded-record boundaries,
single-attempt marker publication, shared submission admission, and exact-runtime quarantine when
commit or installation is uncertain. Paginated mutation remains `thread/revert`; neither rewrites
the user's working files. Retained terminal evidence, explicit checkpoint resume metadata,
count-only compatibility, and source-segment coordinates survive replay, forks, and migration.
Entry points are `core/src/session/rollback.rs`, `core/src/session/submission_admission.rs`,
`history/src/rollout.rs`, `rollout/src/recorder_barrier.rs`, and
`app-server/src/request_processors/thread_rollback.rs`. Derived projection versions are fork-owned
cache state, not amendments to released upstream migrations.
`tui/src/app/legacy_prompt_edit.rs` and `tui/src/app_backtrack/prompt_target.rs` bind edits to
canonical prompt identity, preserve drafts, order the canonical reset after transport events,
and keep an uncertain conversation read-only without disabling unrelated navigation or threads.
Recorder acknowledgement retains the file-flush contract, not a new fsync guarantee. Generated
protocol output and executable qualification remain pending for the completed rebase.

### Shared transcript Review and canonical navigation

Ownership anchor: `feat(tui): share concise transcript review and canonical target navigation`.
The existing `tui/src/transcript_view/review.rs` owns Review/Full mode and chronological targets
for both the owned viewport and inline overlay. Canonical message phase and structured patch
items supply navigation identity; full source, search, selection, voice ownership, and historical
Full previews retain their distinct contracts. Prompt edits require painted content and the
existing canonical rollback/revert safeguards. Copy-on-select and primary selection follow the
current client settings, and fullscreen session tips are not duplicated into transcript rows.
Entrypoints include `tui/src/app/owned_transcript.rs`, `tui/src/app_backtrack.rs`, and
`tui/src/pager_overlay/transcript.rs`. Snapshot and executable qualification remain pending.

### Fail-closed human command approval deadlines

Ownership anchor: `feat(approvals): enforce fail-closed human command deadlines`.
`core/src/session/command_approval.rs` and `request_command_approval.rs` own the monotonic
deadline, exact-turn generation, and one-use response claim. The private queued submission
retains upstream residency and trace/turn lineage while preserving rollback quarantine.
`app-server/src/command_execution_completion.rs` validates exact actor/listener/lifecycle receipts
after output backpressure; callback IDs alone never establish root-command authority.
The TUI retains the first receipt through buffering and replay, and expiry cannot approve work
or act on a later queued request. Absent timing remains untimed. Guardian, hooks, patch approvals,
and permission requests retain their separate policies. Human analytics uses structured trigger
fields, not opaque callback-ID presence. Generated contracts and executable qualification are pending.

### Exact-instance background child completion

Ownership anchor: `feat(multi-agent): persist and surface exact-instance background completions`.
`core/src/agent/control/presentation.rs` binds terminal results, accepted delivery, and wait claims
to the exact child and parent runtime generations. The shared controller API remains separate from
these local capabilities. `core/src/session/sub_agent_completion.rs` and `durable_context.rs` own
single-attempt canonical publication, distinct volatile receipts for ephemeral runtimes, and
primary enqueue; neither a readable record nor a closed channel proves writer closure or client
observation. Accepted workers survive caller cancellation, and uncertain publication quarantines
the original runtime rather than retrying or rebinding it.
Prepared envelopes retain canonical source metadata across mailbox consumption, stale compaction,
and replay. Pending context stays out of live history until consumption. The existing shutdown
path drains accepted work before closing publication, while removal seals the exact old actor.
`thread-store/src/completion_artifacts.rs` preserves original rollback coordinates and frozen
lineage boundaries; private typed provenance governs public projection. Shared TUI previews keep
full raw text without marking the parent's answer complete. The catalog-gated root async message
tool complements, rather than replaces, upstream asynchronous questions. Later response-observation
and user-spawn policy integrations must reconcile these owners, not duplicate their state.
Protocol generation, snapshots, cross-platform tests, and executable qualification remain pending.

### Restore recorded agents through the live owner

Ownership anchor: `feat(multi-agent): restore recorded agents through their live owning control`.
Native V2 child restoration requires its recorded direct parent, exact runtime/tree identity,
and acknowledged graph publication. Options-based and dedicated resume routes share that
boundary; root recovery retains a coherent surviving tree instead of constructing a second
registry. Host controllers keep their shared operation facade and do not fall through to native
restoration. Captured environment, instruction, execution-policy, and MCP authority cannot be
retargeted by resolving the parent UUID a second time. Explicit developer instructions, including
empty strings, stay distinct from inherited settings and saved workspace hints.
`core/src/agent/control/restore_*`, `resume.rs`, and `thread_manager/v2_spawn_resume.rs` own
checked initialization, metadata replacement, lifecycle exclusion, and manager-local recovery
fences. Upstream residency leases and bounded graph-order metadata reads remain in place.
Lossless status observations retain terminal transitions without gaining completion-delivery
ownership; V1 standalone adoption affects only later live turns. The TUI keeps cold selections
read-only until a live command, preserves undelivered input on failure, and uses canonical
completion provenance. Entry points are `tui/src/app/thread_resume.rs` and the shared collab
metadata renderers. Source-authored regression coverage is not executable qualification;
formatting, generated artifacts, and remote validation remain pending.

### Bounded process output and terminal replay

Ownership anchor: `fix(unified-exec): retain bounded output across polling and terminal replay`.
`core/src/unified_exec/process.rs`, `async_watcher.rs`, and `head_tail_buffer.rs` retain each
received chunk in atomic polling/transcript buffers, independently of the exclusive live MPSC
consumer. Exit, producer closure, output-drain completion, and stdin ownership remain separate.
Post-exit draining has an inactivity timeout and absolute limit; incomplete capture is explicit,
and terminal output/token estimates include the final acknowledged drain. Remote combined-byte
offsets locate replay omissions without interpreting missing legacy offsets as gap evidence.
`exec-server/src/local_process.rs` and `client_recovery.rs` preserve native retention/reconnect
ordering, bounded chunk counts, sandbox attribution, and old peers' wire compatibility.
Runtime cancellation can retain cleanup independently while returning one aborted tool outcome;
previously claimed terminal results, timing, and one result-ready event remain authoritative.
TUI command rendering and closed-child navigation retain full source, immediate parent identity,
read-only hydration, cached permission choices, writer quarantine, and server model authority.
Generated protocol output, complete fixture qualification, and execution tests remain pending.

### Exact-turn response observation

Ownership anchor: `feat(multi-agent): bind response observation to exact admitted turns`.
The V1 lifecycle tools share the compact `w` policy, with explicitly requested complete
commentary, passive or waking finals, and transcript-only finals kept distinct. Native
`LocalAgentRuntime` observation state remains separate from the shared controller operations.
Initial spawn and later input retain exact parent/target instances, residency, admission
boundaries, current configuration ownership, and the original provisional cleanup path.
`core/src/agent/control/response_*` and `presentation/response_observation/` own independent
observer policy, exact-turn binding, accepted receipt ownership, and monotonic aggregation.

`core/src/session/response_observation/delivery/publication.rs` uses the current canonical
publication owner, retained-source envelopes, Code Mode ordering, MCP attribution, and live
transcript events. Only the exact finishing input recorder may consume against its own taskless
terminal reservation; unrelated background delivery waits. Wait publication shares upstream
item timing and lifecycle hooks. Dropped receipt waiters do not cancel owned work, and lost
workers fail closed. Canonical audit survives supported rollback and compaction without
recreating cold subscriptions or manufacturing an accepted receipt from history text.

The public collab fields are nullable observation metadata, not new client authority. Legacy
event mirrors cannot erase newer canonical policy fields, while later canonical items can clear
them. The TUI renders routed commentary and observer-relative final visibility through shared
source-preserving previews; wake indicators never activate subscriptions or revive runtimes.
Full first-commentary delivery remains the explicitly requested, product-reviewed size exception.
Remote compilation, tests, schemas, complete snapshots, and final owning-commit formatting remain
outstanding; source review and authored regression cases are not executable qualification.

### User-controlled dispatch and durable aliases

Ownership anchor: `feat(multi-agent): add user-controlled agent dispatch and durable aliases`.
Explicit user commands resolve root-scoped aliases and canonical UUIDs, spawn configured roles,
prompt or resume existing agents, inspect transcripts, and control future response observation.
The native `LocalAgentRuntime` owns these capabilities; a selected host controller does not
silently fall through to local user control. Generic host resume and shared model operations
retain their existing interfaces. Explicit user history forks differ from autonomous delegation
only at the documented history-inheritance and depth gates, not at permissions or environment
authority. Entrypoints are `core/src/agent/user_control/`, `agent/control/user_*`,
`thread_manager/owned_resume.rs`, and the app-server v2 agent-control processor.

`agent-graph-store` and `state/src/runtime/agent_aliases/` retain root identity, nickname/ref
reservations, tombstones, and exclusive transfer checks. Released migration 10049 remains
immutable; its legacy numbering repair is checksum-qualified, and tombstones use migration
10054. Late ancestry backfill preserves closed edges and unmaterialized Main identity. Sorted
writer reservations and captured lifecycle gates separate graph ownership from a live runtime.
One provisional spawn owner joins the original graph write, preserves history behind published
or uncertain aliases, and never disposes of a child whose input outcome may already be admitted.

Typed input receipts distinguish proven admission, a lost observation cursor, and an unknown
routing outcome; the latter never invents a target turn or authorizes automatic resubmission.
Source control audit and task promotion use the current single-attempt publisher and prepared
source envelopes. Task metadata, identity, and adjacent canonical evidence remain unchanged
across checkpoint retention; new MCP attribution belongs on replacement-owned records, not an
acknowledged task. Abandoned publication workers quarantine the exact source rather than
retrying or compensating already accepted target work. Source audit is not model progress.

The TUI retains structured inputs, native audio markers, source-relative draft recovery,
read-only transcript inspection, external-writer guards, current reconnect reconciliation,
dictation/voice ownership, and launch-time screen policy. The scoped-reply owner below replaces
TUI-local held prompts with a process-lifetime backend queue; that framework is not attributed
to the original dispatch owner. Formatting, remote compilation, tests, generated contracts,
and complete snapshot qualification remain outstanding.

### Scoped replies, queued input, and close-response receipts

Ownership anchor: `feat(multi-agent): add scoped replies and target-owned queued turns`.
Reply grants remain exact-instance and turn-scoped capabilities, not permission to interrupt,
adopt, or control another agent. `agent/control/input.rs` preserves genuine user, delegated,
and attributed input separately. `agent/control/user_dispatch.rs` keeps explicit non-admission,
positive admission with a degraded observation receipt, and unknown enqueue outcomes distinct.
The shared controller retains its operation and capacity interface; private queues, reply state,
wait commentary, and completion grants stay with the native runtime. Unsupported host queue
operations return an error rather than exposing an unrelated native queue.

`agent/turn_queue.rs` and `agent/control/turn_queue.rs` own one target FIFO, reversible pending
cancellation, and the irreversible admission claim. Source lifecycle locks, residency leases,
close, and transfer fence queue workers. Accepted work is not resubmitted after uncertain
acknowledgment. Startup retains the exact queue metadata until prompt publication, including
interruption while startup is pending. Canonical prompt recording preserves captured heartbeat
origin, client identity, input order, complete source envelopes, and distinct visible presentation.

`state/completion_context.rs` owns one full-envelope positive receipt inventory used by native
and observed completion publication, including later close replay. Durable store evidence and
explicit runtime-only receipts remain different scopes; model-history removal does not erase
a settled receipt or create permission to replay it. Wait commentary uses the shared canonical
publisher, source metadata, and current turn lifecycle rather than a second insertion path.
Lossy legacy protocol mirrors cannot grant scoped replies or activate historical queue state.

The TUI projects the backend queue without executing a second local dispatcher, retains pending
steers for positive receipt reconciliation, and never automatically resends uncertain input.
Queue editing refuses attachments that cannot be safely reconstructed in the composer before
deleting the original entry, including native audio and executor-owned or file-backed images.
Cold history retains audit and transcript content but recreates neither a queue nor live grants.
Source-authored regression cases still require remote execution; schemas, complete snapshots,
and batched owning-commit formatting remain pending.

### User-controlled delegation and role capabilities

Ownership anchor: `fix(multi-agent): keep delegation policy user-controlled`.
`core/src/session/multi_agents.rs` selects configured delegation hints or the explicit reasoning
policy, not catalog delegation metadata. Configured text remains complete and intentional empty
text remains authoritative. `prompts/src/multi_agent_instructions.rs` keeps catalog role fallback
and current capability-sensitive composition while reporting the configured fork default and
requiring explicit instructions for model/reasoning overrides. History inheritance is a separate
user-authorized choice.

Role MCP registrations merge through constrained configuration and the existing catalog without
replacing provider, managed command identity, permissions, or notification authority. The
`agent/control/restore_environments.rs` owner retains exact executor and attachment checks,
materialized local intersections, and only proven managed read-only reductions remotely. Remote
filesystem proofs use the selected executor's convention, never host-native path assumptions.
Cold-resume fixtures retain real residency and request correlation, current foreign-environment
restart support, canonical flushes, and complete provider/MCP comparisons. Source regression
coverage is not a passed executable result; formatting and remote qualification remain pending.

### Read-only cross-home lineage copies

Ownership anchor: `fix(fork): copy paginated lineage across Codex homes`.
`thread-store/src/local/fork_copy.rs` reads opened source representations at captured complete-line
limits; inherited prefixes retain their original immutable rollout IDs and byte/ordinal cutoffs.
Compressed sources use anonymous decoded readers and backup-only sources require existing journal
authority. Symlink resolution selects the actual source home. Active-home SQLite rows cannot
replace external metadata, and source files are never recovered in place or rewritten.

Exact rollback uses original decoded coordinates before flattening. The destination owns its one
session header and copied history, including copied-deferred initialization. Both leading delivery
metadata and following observation/task snapshots retain their original evidence relationship:
`fork_copy_provenance.rs` rejects copies that would create new proof by deleting a source boundary.
The guard compares complete envelopes and snapshots, not IDs alone, and does not convert audit
history into a positive writer acknowledgment or a live subscription.

`core/src/thread_manager/external_fork.rs` separates persisted source identity from authority to
look up a local runtime. External root forks keep explicit destination providers and saved history
without inheriting a coincident local UUID's provider snapshot, runtime fallback, originator, or
attachment membership. External goal deferral is rejected before allocating a destination;
ordinary in-home goal inheritance and coordinated reference forks remain unchanged. The public
fork/cold-resume fixture and source-authored namespace/provenance regressions require remote
execution. No local test or compiler result is implied by this source integration.

### Detached user-shell process control

Ownership anchor: `feat(user-shell): add explicit long-running process control`.
Human shell commands retain their own process lifetime without owning or faking a model turn.
The configured zero default means no wall-clock deadline; an explicit request timeout of zero
retains its immediate-expiry meaning. Local execution retains the selected shell, environment,
sandbox metadata, policy, bounded output, and model-input attribution. Remote executor commands
are rejected rather than redirected to the host.

`unified_exec/user_shell_registry.rs` shares process IDs with the current process manager and
keeps exact call-ID removal separate from cancellation. Its tracked producer is admitted before
first poll under the shutdown gate; cancellation is registered before shell-snapshot preparation.
Session shutdown cancels and drains the actual output/publication producers before closing history.
Stopping a process, removing a registry entry, and publishing its final records are not equivalent
receipts. A model interruption does not cancel an independently admitted shell command.

Detached command audit uses completed standalone activity without synthetic model turn boundaries.
Current orphaned agent-presentation provenance and bounded semantic-compaction replay remain intact.
The TUI accepts output only for a live matching user-shell call while the model is idle, retains
that command across model finalization, and waits for its real completion rather than rendering
a false failure. `/ps` preserves multiline source and reports process IDs; bare `/stop` retains
stop-all semantics, while argument completion selects a specific positive process ID. Existing
MCP completion and App-owned realtime controls are not replaced by older helper implementations.

Source tests cover producer cancellation/drain, real shutdown publication, detached audit isolation,
live idle output, and model-failure independence. They have not run. An inherited long-command
wrapping snapshot is explicitly assigned to its earlier preview owner for final regeneration;
owner90 adds only the new process-ID contract. Later user-shell queued completion policies remain
with their separate feature owner. Formatting, generated contracts, and executable validation
remain pending.

### Queued user-shell completion ownership

Ownership anchor: `feat(user-shell): add queued completion policies`.
The per-thread shell queue reserves submission order independently of model turns and agent input
queues. Its cleanup owner is captured before an execution future can first be polled, so rejected
or abandoned launches cannot strand later queued commands or a pending wake. The process manager's
tracked producer admission and shutdown drain remain authoritative through result publication;
requesting cancellation alone does not certify process exit or writer completion.
`session/user_shell_delivery.rs` keeps passive, wake, and presentation-only results distinct.
Passive and wake context is canonically appended, acknowledged, and installed before scheduling.
Only a continuable active invocation accepts the local `UserShellContextReady` signal; otherwise
a wake uses ordinary next-turn admission with `user_shell_wake` attribution. This nonserializable
signal contains neither payload nor authority, and consuming or discarding it cannot republish or
lose the acknowledged output. Prepared history envelopes retain the current source and privacy
handling. A failed canonical publication admits no wake and is not retried. Typed completion history,
old-record compatibility, local executor authority, and detached live output remain intact.
User-shell `!w:` is implemented separately from the still-proposed model-tool terminal wake.
The new queue, cancellation, scheduling, and projection tests are source-authored only; remote
execution, generated contracts, and final-source snapshot review remain outstanding.

## Integration boundaries and deferred work

- Upstream unified exec supersedes the legacy `shell`/`shell_command` handlers and the fork's `feat(config): add default shell command timeout` carry. `exec_command_timeout_ms` is retired, not a missing feature to replay or an alias for a yield window or user-shell deadline. Normal resumable `exec_command` yields output without terminating the process; the managed-policy one-shot fallback retains its separate per-call deadline. Keep the maintained yield-window, poll-cap, and human user-shell timing capabilities above. Historical commits and backup refs remain valid evidence; remove the old setting from user configs after promotion rather than restoring the handlers or rewriting historical backups.
- The pinned target already supplies the current realtime voice/WebRTC owners. Historical restoration and Linux-audio patches were not transplanted wholesale; editable browser-authenticated dictation is a separate downstream capability.
- Preserve the current Paginated defaults for new durable TUI/exec sessions and explicit/stored Legacy compatibility. Historical default-switch subjects in the replay stack do not override the integrated runtime contract.
- Stable and experimental app-server schemas/bundles, configuration descriptions, persisted-history/embedded Python SDK artifacts, and Bazel dependency locks require coordinated final-source remote regeneration. Historical generated artifacts are not evidence that the current source contracts have been validated.
- Canonical publication ambiguity quarantines the exact session. Readable history is not acknowledgment, accepted receipts cannot be retargeted, and process-local ownership fences do not promise crash-atomic transfers or fsync durability.
- The terminal-wake and workspace-root AGENTS documents remain proposals. Scoped reply routes, backend queue-input policy, and close-response replay are maintained by their separate integrated owner; they do not implement terminal-process wake scheduling.
- The current rebase records per-owner source reviews and explicit deferred repairs. Batched formatting fixes must be folded into their owning downstream commits after replay; no local compilation, tests, or generation are authorized. Previously built 0.156.1 binaries are not qualification of this source.
- Release-version changes remain at the final release owner; this inventory does not announce the historical 0.147.0 release as the integrated tip.

## Update Protocol

- Compare the resulting stack against the pinned integration base, not local `main` or a rolling upstream branch. During replay use `HEAD`; after integration use the completed fork branch.
- Keep one row per maintained capability and update its entrypoints and exact semantic commit subjects when ownership moves, commits are reworded, or features are split or squashed.
- Compare upstream behavior and tests before narrowing or removing a carry; record explicit drops and inherited baseline behavior separately.
- Keep stabilization-only integration repairs, release-only version changes, and transient CI repair attempts distinct from maintained capabilities.
- Reconcile documentation and generated contracts at their coordinated checkpoint. A history rewrite alone does not execute tests or validate every intermediate commit.

## Review Checklist

Before declaring the final ledger current:

1. Audit retained, dropped, and later replay owners against the original manifest and pinned integration base.
2. Verify every listed entrypoint and semantic subject against the integrated tree and history.
3. Reconcile configuration and app-server documentation with remotely regenerated schemas and SDK artifacts.
4. Exercise resume, fork, rollback, ownership transfer, compaction, and persisted-session compatibility through remote validation.
5. Record actual remote verification and release workflow results for the final tip; no passing result is claimed here.
