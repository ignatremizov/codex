# Codex Fork Ledger

This is the living maintenance map for capabilities carried by `fork` beyond its audited upstream integration base. It is a capability inventory, not a chronological changelog or a requirement to edit documentation in every feature commit.

The 0.160 integration is pinned to upstream source commit `cb7799623b2241f536d799f2d46259148fb987ce`, including its maintenance backports. The upstream release-notes/version commit `a956835d020762cb2b570053af06f643a11c0ecc` is deliberately excluded. Neither rolling `upstream/main` nor local `main` defines the integration base. Keep release-version changes at the final release owner.

The replay manifest inventories the 130 original downstream commits in `f1b21bb2931f86819e32df0cafa1bf69570d84ee..ac0f63abe470b8fa83d7852db7e5d39282c88434`. Initial replay, source-owner repairs, qualification-tail consolidation, and batched formatting reconciliation are complete. Reviewed dispositions preserve every original change or record its existing defining owner; source-only history cleanup is not executable qualification. Inspect the resulting stack with `git log --reverse cb7799623b2241f536d799f2d46259148fb987ce..fork`. The complete pre-rebase ledger remains the policy-preservation checklist, and final-source schemas, snapshots, dependency locks, and remote tests remain separate validation gates.

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

This checkpoint includes compact replies to upstream asynchronous questions, captured credential profiles, durable owning-subtree unload, batch input, owned sleep progress, conditional mailbox finals, and invocation-owned terminal waits. It also retains live sibling/mailbox receipts, FIFO asynchronous presentation, shared agent identity styling, receiver-selected input delivery, user-controlled observation and delegation, acknowledged reply routes, canonical history, and source-preserving transcript navigation. Unimplemented proposals remain proposals. Entrypoints and semantic subjects name the integrated source; neither this inventory nor formatting success claims executable validation.

| Capability | Kind | Purpose | Primary fork entrypoints | Required upstream seams | Commits |
| --- | --- | --- | --- | --- | --- |
| Compact asynchronous question replies | Efficiency | Add thread-local question references to upstream's asynchronous question tool, shorten recognized replies only in disposable model input, and recover numbering from complete canonical artifacts across compaction and resume. Preserve original transport, replay, question dismissal, and unknown or mixed-text replies. | `codex-rs/context-fragments/src/answered_question.rs`<br>`codex-rs/core/src/session/async_questions.rs`<br>`codex-rs/core/src/context_manager/history.rs`<br>`codex-rs/core/src/tools/handlers/request_user_input_async.rs` | Existing asynchronous question identity and UI, source-aware request envelopes, raw artifact lineage, Legacy/Paginated event persistence, and final dependency-lock qualification | `feat(context): compact async question replies with durable short references` |
| Build-profile-independent display fixtures | Release | Keep layout-sensitive unit snapshots stable across source and release builds while production headers, status, and update notices retain the actual package version. Runtime update decisions and client/server version checks remain independent from test display normalization. | `codex-rs/tui/src/version.rs`<br>`codex-rs/tui/src/app/history_ui.rs`<br>`codex-rs/tui/src/history_cell/`<br>`codex-rs/tui/src/status/card.rs` | Source-backed startup and clear headers, status copy, optional version footer, full snapshot metadata, and the final packaging boundary | `test(tui): stabilize display versions across build profiles` |
| Durable owning-subtree unload | Capability | Stop a selected task's owning root and loaded spawn descendants only after exact runtimes acknowledge producer drain, persistence, and writer release; retain failed actors and cancelled-spawn cleanup for retry. Graceful TUI exit uses unload, while explicit shared-server disconnect leaves server-owned work running. | `codex-rs/core/src/thread_manager/loaded_subtree.rs`<br>`codex-rs/core/src/session/durable_shutdown.rs`<br>`codex-rs/core/src/unified_exec/shutdown.rs`<br>`codex-rs/core/src/thread_manager/owned_resume_startup.rs`<br>`codex-rs/app-server/src/request_processors/thread_unload.rs`<br>`codex-rs/tui/src/app/exit_lifecycle.rs` | Exact lifecycle and subscription fences, canonical completion ownership, captured startup/controller authority, Code Mode and Guardian teardown, real process exit, and writer-lease release | `feat(lifecycle): durably unload owning subtrees on graceful client exit` |
| Captured credential profiles with shared storage | Capability | Select saved credentials with `CODEX_AUTH_FILE` without relocating configuration, skills, rollouts, or ownership. Preserve provider/API-key precedence, strict browser dictation, and captured reload/login/logout identity; isolate daemon lifecycle and recovery by profile and verify the actual local connection's home and profile before reuse. | `codex-rs/login/src/auth_file_selection.rs`<br>`codex-rs/login/src/auth_profile.rs`<br>`codex-rs/login/src/auth/manager.rs`<br>`codex-rs/app-server-daemon/src/launch_options.rs`<br>`codex-rs/tui/src/auth_profile_connection.rs` | Config/bootstrap capture, account-bound network policy, model-catalog caching, daemon package/PID ownership, actual connection verification, and current TUI startup/reconnect boundaries | `feat(auth): select credential profiles while sharing thread storage` |
| Batch agent input with shared presentation | Capability | Send one typed input to a nonempty array of canonicalized recipients, deduplicating aliases in first-seen order and retaining independent direct, queued, mailbox, and failure outcomes. Preserve partial admissions on cancellation and show the shared input once, without a second retry ledger or a false delivery guarantee. | `codex-rs/core/src/tools/handlers/multi_agents/send_input_batch.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents/send_input_admission.rs`<br>`codex-rs/core/src/agent/control/message_audit.rs`<br>`codex-rs/protocol/src/collab_input.rs`<br>`codex-rs/tui/src/multi_agents/send_input_batch.rs`<br>`codex-rs/tui/src/app/replay_filter.rs` | Native attribution, shared interrupt facade, retained admission workers, canonical recipient history, typed legacy/API conversion, and source-owned live/replay presentation | `feat(agents): support batch send_input with shared lifecycle presentation` |
| Exact delivered-final recovery | Compatibility | On resume suppress only an existing completed reconciliation candidate proven delivered by original-coordinate, rollback-filtered canonical evidence for the exact receiver, child, target turn, and response identity. Without that proof, preserve unseen, conflicting, empty, and later same-text results; errors remain eligible, and history never creates a subscription. Durable model-hidden root oversight remains separate from nonpersistent peer receipts. | `codex-rs/core/src/agent/control/resume_delivery.rs`<br>`codex-rs/core/src/agent/control/response_observer.rs`<br>`codex-rs/core/src/agent/control/presentation/root_completion_audit.rs` | Strict receiver-owned history, original adjacency, exact-instance observation, terminal reconciliation, and canonical root-oversight ownership | `fix(agents): prevent duplicate finals across delivery, sampling, and resume`, `feat(multi-agent): surface unobserved child conclusions in Main` |
| Shared rich agent identity headers | Observability | Render confirmed nicknames, roles, task paths, numeric refs, and partial model/reasoning settings consistently across live and cold presentation; retain independently authoritative task-path clears and successful control mappings. Terminal-adapted nickname colors remain decorative rather than identity or authority. | `codex-rs/tui/src/multi_agents/identity_header.rs`<br>`codex-rs/tui/src/agent_color.rs`<br>`codex-rs/tui/src/chatwidget/collab_metadata.rs`<br>`codex-rs/tui/src/thread_transcript/agent_metadata.rs` | Trusted alias snapshots, fresh-spawn settings, partial metadata, client-local rendering, and terminal capabilities | `feat(tui): unify agent identity styling and fresh spawn references` |
| Live sibling and mailbox acceptance receipts | Observability | Present fresh sibling acceptance independently of payload delivery or model visibility, retain root-side mirrors through in-process refresh without canonical root writes, and place asynchronous notices after complete authored answer/plan source while preserving explicit communication provenance and notice FIFO. | `codex-rs/core/src/agent/control/mailbox_input.rs`<br>`codex-rs/core/src/agent/control/message_audit.rs`<br>`codex-rs/protocol/src/mailbox_delivery.rs`<br>`codex-rs/tui/src/app/replay_filter.rs`<br>`codex-rs/tui/src/chatwidget/inter_agent_transcript.rs`<br>`codex-rs/tui/src/chatwidget/interrupts.rs` | Immutable acceptance retries, exact live root ownership, reserved-ID sanitization, typed attachment conversion, source consolidation, and live-versus-cold history boundaries | `feat(tui): distinguish sibling sends and mailbox acceptance receipts` |
| Mailbox outcome projection and available-agent navigation | Observability | Distinguish fixed-claim delivery outcomes from canonical acceptance and project only recognized inventory text through the receiving request's advertised refs. Keyboard cycling skips unavailable members without deleting historical picker rows or taking a replay-only attachment after failed session hydration. | `codex-rs/core/src/context/check_mail_result.rs`<br>`codex-rs/core/src/context/mailbox_inventory_projection.rs`<br>`codex-rs/thread-store/src/mailbox_inventory_presentation.rs`<br>`codex-rs/tui/src/app/agent_cycling.rs`<br>`codex-rs/tui/src/app/agent_cycle_order.rs` | Immutable mailbox claims and notification frontiers, bounded legacy/current inventory representations, disposable sampling/compaction input, root ownership, live attachments, session/profile refresh, and receiver restoration | `feat(agents): clarify mailbox outcomes and navigate available agents` |
| Receiver-scoped compact agent identities | Efficiency | Hydrate the receiving root's bounded current identity map, including closed addressable members, and project routine V1 request envelopes to those refs without rewriting canonical UUID/send-time evidence. Keep model-facing send receipts admission-only and resume status distinct from a previous final answer. | `codex-rs/core/src/agent/control/identity_snapshot.rs`<br>`codex-rs/core/src/context/agent_envelope_projection.rs`<br>`codex-rs/core/src/context/world_state/agent_identities.rs`<br>`codex-rs/core/src/context/world_state/agent_identity_delta.rs` | Native root identity, request-scoped world state, canonical compaction evidence, rich-input attribution, and V1 tool output schemas | `feat(agents): hydrate compact receiver-scoped identities without losing canonical attribution` |
| Local home identity and copyable rollout paths | Observability | Show the resolved local TUI home as an optional basename in the status line and a full path in `/status`; print the server-reported rollout path through `/rollout-path` or `/rollout` without guessing storage authority; retire status copy targets after accepted input or a new turn without reviving them on a delayed refresh. | `codex-rs/tui/src/status/storage.rs`<br>`codex-rs/tui/src/chatwidget/status_surfaces.rs`<br>`codex-rs/tui/src/chatwidget/status_controls.rs`<br>`codex-rs/tui/src/chatwidget/slash_dispatch.rs` | Client-local configuration, status preview/style selection, Arc-backed status refresh, accepted input lifecycle, and server-reported current rollout metadata | `feat(tui): expose local Codex home and copyable rollout paths` |
| Durable receiver-selected input mailbox | Capability | Accept typed user or attributed V1 agent mail without steering or claiming consumption; select fixed batches through direct `check_mail` or targeted waits; recover result/context/presentation receipts from the receiver's own canonical history; persist user-controlled send settings independently of transient subscriptions; and admit bounded, coalesced inventory wakes behind ordinary queued work. | `codex-rs/core/src/agent/control/mailbox_input.rs`<br>`codex-rs/core/src/session/mailbox.rs`<br>`codex-rs/core/src/session/mailbox_publication.rs`<br>`codex-rs/core/src/context/mailbox_inventory.rs`<br>`codex-rs/thread-store/src/mailbox_artifacts.rs`<br>`codex-rs/state/src/runtime/queued_items/mailbox.rs`<br>`codex-rs/tui/src/chatwidget/mailbox.rs` | Native/host control boundary, durable permission transactions, canonical publisher and input ordering, idle admission, raw receiver-owned history, rich TUI draft recovery, and current v2 protocol | `feat(mailbox): add durable receiver-selected input delivery` |
| Canonical model slugs and stable accents | Compatibility | Show canonical active-model slugs in session headers and footers while retaining catalog labels in pickers and historical analytics; preserve stable thread-name and title accents without reverting newer layouts, warning controls, or key notation. | `codex-rs/tui/src/bottom_pane/status_line_style.rs`<br>`codex-rs/tui/src/chatwidget/session_flow.rs`<br>`codex-rs/tui/styles.md` | Current model selection, status/footer rendering, session startup, TUI snapshots | `revert(tui): restore canonical model slugs and stable accents` |
| Private Guardian screenshot evidence | Compatibility | Preserve missing private-review screenshot evidence with original provenance after compaction without replaying already admitted text or weakening reviewer input budgets and source isolation. | `codex-rs/core/src/context/node_repl_review_evidence.rs`<br>`codex-rs/core/src/guardian/input_budget.rs`<br>`codex-rs/core/src/guardian/review_session_images.rs` | Private review lifecycle, current screenshot selection, compaction, input budgeting | `fix(guardian): preserve private screenshot evidence across compaction` |
| Attributed V1 messages and task-path discovery | Capability | Distinguish model-authored agent input from human prompts using compact escaped attribution and separate durable sender/recipient snapshots; assign root-scoped task labels independent of lifecycle ancestry; preserve closed-label uniqueness and report adoption remapping; expose opt-in root-controlled paginated `list_agents` to current and future members without enabling sends or restoring runtimes. | `codex-rs/core/src/agent/control/task_paths.rs`<br>`codex-rs/core/src/agent/control/directory.rs`<br>`codex-rs/core/src/context/attributed_agent_message.rs`<br>`codex-rs/state/src/runtime/agent_aliases/task_paths.rs`<br>`codex-rs/tui/src/history_cell/agent_input.rs` | V1 spawn/send/resume, role/model precedence, alias ownership transactions, typed input and transcript projections, tool availability, TUI selection and completion | `feat(multi-agent): add attributed V1 messages and task-path discovery` |
| Compact `apply_patch` tool guidance | Efficiency | Keep the model-visible patch contract explicit while avoiding redundant per-session instruction tokens. | `codex-rs/core/src/tools/handlers/apply_patch_spec.rs` | Tool schema construction and schema tests | `fix(tools): shorten the apply_patch freeform description` |
| Complete repeated `apply_patch` history | Observability | Preserve every verified update hunk when one patch edits the same file repeatedly so inline summaries, transcript review, and persisted completion events do not show only the final section. | `codex-rs/apply-patch/src/invocation.rs`<br>`codex-rs/tui/src/diff_render.rs` | Apply-patch verification, structured change aggregation, TUI diff rendering | `fix(apply-patch): verify repeated updates as one canonical file change` |
| Unlabelled compact rate-limit percentages | Compatibility | Preserve the compact rate-limit presentation instead of adding repeated window labels to status surfaces. | `codex-rs/tui/src/chatwidget/status_controls.rs`<br>`codex-rs/tui/src/bottom_pane/status_line_setup.rs` | TUI status controls and snapshots | `fix(tui): shorten compact rate-limit percentage labels` |
| `/approvals` compatibility command | Compatibility | Retain the familiar permissions command alongside `/permissions` without conflicting with the separate `/approve` auto-review action. | `codex-rs/tui/src/slash_command.rs`<br>`codex-rs/tui/src/chatwidget/slash_dispatch.rs` | TUI slash-command catalog and dispatch | `fix(tui): restore /approvals as an alias for permission settings` |
| In-place prompt editing by default | Compatibility | Roll a thread back when editing an earlier prompt, preserving the exact selected durable boundary across compaction, SQLite projection rebuilds, resume, and inherited fork prompts while leaving source-preserving prompt forks available through `fork_prompt_edits`. Duplicate rendered copies use turn/item identity; the latest session header and rejected edit draft are retained. | `codex-rs/tui/src/app_backtrack/prompt_projection.rs`<br>`codex-rs/tui/src/app/legacy_prompt_edit.rs`<br>`codex-rs/tui/src/app/event_dispatch.rs`<br>`codex-rs/core/src/session/handlers.rs`<br>`codex-rs/protocol/src/protocol.rs`<br>`codex-rs/thread-store/src/local/thread_history_materialization.rs` | App-server rollback, durable rollout markers, paginated thread-history projection, TUI thread/event lifecycle, feature configuration | `feat(tui): add opt-in source-preserving prompt edits`, `fix(rollback): preserve exact durable Legacy thread boundaries`, `fix(tui): restore prompt editing across forked histories` |
| Deferred tool discovery in Lite and Code Mode | Capability | Keep `tool_search` reachable when V1 tools are deferred by Responses Lite and expose ranked deferred MCP discovery inside Code Mode without injecting every schema up front. | `codex-rs/core/src/client.rs`<br>`codex-rs/core/src/tools/handlers/tool_search.rs`<br>`codex-rs/tools/src/code_mode.rs` | Responses request serialization, tool routing, Code Mode declarations/runtime | `fix(client): keep Lite tool search reachable for deferred V1 agents`, `fix(code-mode): expose ranked deferred tool discovery` |
| Non-persistent raw response traces | Efficiency | Preserve opt-in transport diagnostics while excluding full Responses API payloads from the SQLite log sink to reduce sensitive-data retention and insert/prune churn. | `codex-rs/codex-api/src/sse/responses.rs`<br>`codex-rs/codex-api/src/endpoint/responses_websocket.rs` | State log filtering | `fix(logging): exclude raw response events from SQLite and feedback` |
| Complete role and model selection for subagents | Capability | Expose built-in roles without requiring custom role configuration, allow MultiAgent V2 to spawn any loaded catalog model regardless of its default multi-agent tag, keep collaboration tools available to those V2 children, keep explicit model and reasoning overrides independent of full-history inheritance in both agent tool versions, and let configured roles replace inherited base instructions from a role-local non-empty file while preserving separate developer instructions and custom provenance. Cross-version role/model precedence remains a remote executable qualification requirement. | `codex-rs/core/src/agent/role.rs`<br>`codex-rs/core/src/agent/child_config.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`<br>`codex-rs/core/src/tools/spec_plan.rs` | Spawn tool schemas, model catalog validation, child tool exposure, history inheritance, role-local instruction loading, TUI app-server session configuration | `fix(multi-agent): expose built-in roles and retain authoritative runtime schemas`, `fix(multi-agent): resolve child model overrides across catalog runtime tags`, `feat(multi-agent): require user authorization for inherited history`, `feat(multi-agent): add per-spawn model and reasoning overrides` |
| Interactive and resumable subagent threads | Compatibility | Keep parent-owned and V2 subagent threads directly inspectable and interactive, revive replayed or cold-resumed children through their owning control plane, and preserve routing, identity, completion delivery, graph state, and parent-return behavior. | `codex-rs/core/src/agent/control/spawn.rs`<br>`codex-rs/core/src/thread_manager/v2_spawn_resume.rs`<br>`codex-rs/tui/src/app/thread_resume.rs`<br>`codex-rs/tui/src/app/agent_navigation.rs` | App-server direct input, agent ownership and registry state, V1 adoption, V2 graph restoration, TUI thread navigation | `fix(tui): allow direct child-thread input while preserving writer quarantine`, `fix(app-server): allow direct child input and caller-controlled cold resume`, `feat(multi-agent): restore recorded agents through their live owning control`, `fix(app-server): keep resumed agent threads directly interactive` |
| Configurable and auditable V2 agent messaging | Observability | Select encrypted, encrypted-with-audit, or plaintext delivery; retain complete readable assignments where permitted; and preserve readable communication in rollout traces without confusing audit text with transport correlation. | `codex-rs/core/src/agent_communication.rs`<br>`codex-rs/core/src/tools/handlers/multi_agents_v2/`<br>`codex-rs/rollout-trace/src/reducer/tool/agents.rs` | Thread config locking, agent registry/control, spawn/send/follow-up schemas, rollout reduction | `feat(config): select the multi-agent V2 message delivery policy`, `feat(multi-agent): separate readable audit from V2 message delivery`, `feat(rollout-trace): retain readable content on agent interaction edges` |
| Configurable context inheritance for V2 spawns | Efficiency | Default new V2 subagents to no parent turns while allowing `none`, `all`, or a bounded turn count through `features.multi_agent_v2.default_fork_turns`; inherited history additionally requires the user's `agents.allow_history_forks` authorization. Explicit history forks keep parent audit records canonical while excluding stale parent-agent delivery envelopes from child instructions. | `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`<br>`codex-rs/core/src/agent/control/spawn.rs` | Feature configuration, spawn schema and parsing, fork-history filtering | `feat(multi-agent): require user authorization for inherited history` |
| Auditable multi-agent transcripts | Observability | Show useful multi-line task prompts and completed wait responses with independent configurable caps, preserve readable V2 task messages in parent and child histories, surface attributable inter-agent communication through live and resumed transcripts without exposing encrypted content, and resolve collab labels from thread-wide canonical nickname/role metadata across paginated pages and complete exports. | `codex-rs/tui/src/multi_agents.rs`<br>`codex-rs/tui/src/thread_transcript.rs`<br>`codex-rs/tui/src/app/history_pagination.rs`<br>`codex-rs/tui/src/app/thread_routing.rs`<br>`codex-rs/tui/src/chatwidget/tool_lifecycle.rs`<br>`codex-rs/app-server-protocol/src/protocol/v2/item.rs`<br>`codex-rs/app-server/src/request_processors/response_item_transcript.rs`<br>`codex-rs/thread-store/src/local/thread_history_materialization.rs` | Canonical collab item conversion, TUI tool-history rendering and receiver cache, app-server thread history and notifications, durable rollout reconstruction, fork-owned history projections | `feat(tui): configure source-preserving agent prompt and response previews`, `feat(multi-agent): preserve readable task prompts in parent activity`, `feat(multi-agent): publish durable inter-agent transcripts with typed provenance` |
| Durable background subagent completions | Observability | Show terminal v1 and v2 child results immediately when no active wait owns presentation, retain their full model-visible context and canonical transcript rows through cancellation, rollback, shutdown, compaction, pagination, and cold resume, and allow a later explicit wait to render independently. Unobserved later live V1 conclusions additionally receive model-hidden root oversight, without implicit model delivery, wake, or subscription. | `codex-rs/core/src/session/sub_agent_completion.rs`<br>`codex-rs/core/src/agent/control/presentation/root_completion_audit.rs`<br>`codex-rs/core/src/agent/control/presentation.rs`<br>`codex-rs/protocol/src/sub_agent_completion.rs`<br>`codex-rs/thread-store/src/completion_artifacts.rs`<br>`codex-rs/tui/src/multi_agents/background_completion.rs` | Agent status publication, parent Session lifecycle, rollout persistence and reconstruction, app-server history projection, TUI collab rendering | `feat(multi-agent): persist and surface exact-instance background completions`, `feat(multi-agent): surface unobserved child conclusions in Main` |
| Event-driven subagent response observation | Capability | Let V1 lifecycle calls request the first complete commentary, subscribe to an exact target turn's final reply, or explicitly avoid a new final subscription through compact `w` flags, while coordinating wait presentation and independent exact-instance observers. Newly installed passive finals release active clock.sleep without starting an idle turn. Delivered audit survives rollback, compaction, recovery, and migration; cold history never recreates pending subscriptions. | `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`<br>`codex-rs/core/src/agent/control/response_observer.rs`<br>`codex-rs/core/src/agent/control/response_delivery.rs`<br>`codex-rs/core/src/session/response_observation.rs`<br>`codex-rs/thread-store/src/local/rollout_migration/canonicalizer.rs`<br>`docs/multi-agent-v1-response-observation.md` | Agent lifecycle tool schemas, exact-turn admission, durable rollout context, completion publication, rollout migration, TUI commentary/final presentation | `feat(multi-agent): bind response observation to exact admitted turns`, `fix(agents): interrupt active sleep on committed passive finals` |
| Running-agent footer and title activity | Observability | Show other running agents in the current root above the composer and animate the configured title activity while preserving Ready/input state, action-required priority, closed/transferred exclusions, and exact-turn response policies. Generic status and picker refreshes update visual liveness only; no new polling or activity registry. | `codex-rs/tui/src/app/agent_activity.rs`<br>`codex-rs/tui/src/app/agent_picker.rs`<br>`codex-rs/tui/src/bottom_pane/agent_activity_footer.rs`<br>`codex-rs/tui/src/chatwidget/status_surfaces.rs` | Existing navigation/root aliases, live lifecycle/status notifications, picker selection, shared footer styling, title configuration | `feat(tui): show running agents above the composer` |
| Native user agent control plane | Capability | Let users inspect, spawn, prompt, queue, interrupt, resume or explicitly adopt by UUID, close, and change one-turn response observation for V1 and V2 agents through `/agent`, using durable root-scoped refs and nicknames while preserving canonical UUID ownership, structured user input, exact-turn delivery, source-side audit history, transcript inspection, and explicit fork modes. Unknown input outcomes require manual reconciliation, never automatic retry. TUI-held prompts remain distinct from target-owned queue acceptance and admitted turns. | `codex-rs/core/src/agent/user_control/`<br>`codex-rs/state/src/runtime/agent_aliases/`<br>`codex-rs/app-server/src/request_processors/thread_processor/agent_control.rs`<br>`codex-rs/tui/src/chatwidget/agent_command.rs`<br>`codex-rs/tui/src/app/agent_control_pane.rs`<br>`docs/tui-agent-control.md` | Agent graph persistence and transfer, app-server v2 control APIs, response observation, fork-history projection, TUI composer and thread routing | `feat(multi-agent): add user-controlled agent dispatch and durable aliases`, `feat(multi-agent): add scoped replies and target-owned queued turns`, `feat(tui): expand agent overview and transcript inspection` |
| User-selected observation endpoints | Capability | Replace an existing final-response subscription through `/agent observe <target> from <observer> ...`, keeping the issuing actor independent from the selected observer. Preserve exact controlled runtime identity, canonical snapshot acknowledgement, directional display, audit-only authored selectors, and truthful unknown outcomes without creating input, subscriptions, or grants. | `codex-rs/core/src/agent/control/user_observation.rs`<br>`codex-rs/core/src/agent/user_control/lifecycle.rs`<br>`codex-rs/app-server/src/request_processors/thread_processor/agent_control.rs`<br>`codex-rs/tui/src/app/agent_lifecycle_control.rs` | Native graph/ownership, selected-observer publication, issuer audit, API optional selectors, source-preserving composer tokens, live and persisted transcript metadata | `feat(multi-agent): select observers in TUI subscription controls` |
| Scoped replies and target-owned queued turns | Capability | Bind optional V1 reply permission to exact live endpoints and admitted target work; share a process-lifetime FIFO between model and user queued input; keep queue acceptance distinct from turn admission, prompt persistence, and response delivery. Explicit close replay reuses acknowledged completion provenance rather than inventing a new receipt. | `codex-rs/core/src/agent/control/scoped_messages.rs`<br>`codex-rs/core/src/agent/turn_queue.rs`<br>`codex-rs/core/src/session/observed_input.rs`<br>`codex-rs/core/src/session/close_response.rs`<br>`codex-rs/tui/src/app/agent_prompt_queue.rs` | Shared controller/native runtime separation, input origins, canonical source publication, startup and interruption, completion receipt scope, residency, host-aware queue APIs, reconnect recovery | `feat(multi-agent): add scoped replies and target-owned queued turns` |
| User-controlled live reply routes | Capability | Let the user enable or disable a V1 target's replies across later turns of its exact live relationship; preserve accepted forward queues and reverse reservations, with acknowledged singleton context and no cold-restored reply authority. | `codex-rs/core/src/agent/control/user_reply_route.rs`<br>`codex-rs/core/src/session/reply_route_publication.rs`<br>`codex-rs/history/src/agent_reply_route.rs`<br>`codex-rs/tui/src/chatwidget/agent_command.rs` | Native controller/runtime ownership, two canonical journals, source admission, compaction/rollback retention, app-server v2 audit, and live-only TUI status | `feat(multi-agent): add acknowledged live user-controlled reply routes` |
| Visible completed compaction | Observability | Expose installed compaction output in TUI, exec, app-server, and JSONL history while preserving current compaction ownership and replacement-history semantics. | `codex-rs/core/src/compact.rs`<br>`codex-rs/core/src/tasks/compact.rs`<br>`codex-rs/exec/src/exec_events.rs` | Core session/turn lifecycle, protocol items, app-server thread history, TUI rendering | `feat(compact): expose completed compaction output across history and clients` |
| User-controlled context automation | Compatibility | Keep token budgeting, private context management, and automatic TUI recaps disabled until the user explicitly enables them; prevent model-catalog metadata from activating hidden work; retain manual `/recap`; and keep the visible `update_plan` tool available unless explicitly disabled. | `codex-rs/core/src/session/token_budget.rs`<br>`codex-rs/core/src/config/mod.rs`<br>`codex-rs/tui/src/app/recap.rs` | Model-catalog defaults, feature/config resolution, world-state instruction filtering, TUI recap scheduling | `fix(context): preserve user-controlled automation defaults` |
| Inspectable remote compaction handoff | Observability | Decode the provider-opaque post-compaction handoff through a locked-down helper, show a live decode phase, and persist server-reported summary token counts without changing replacement history; the user-controlled remote-compaction feature gate remains authoritative even for provider-capable models. | `codex-rs/core/src/compact_handoff_summary.rs`<br>`codex-rs/core/src/compact_remote_v2.rs`<br>`codex-rs/core/src/compact_remote_history.rs`<br>`codex-rs/core/src/tasks/compact.rs` | Agent delegation, compaction protocol/events, rollout reconstruction, app-server notifications, TUI status | `feat(compact): decode installed remote handoffs for display`, `feat(compact): expose live decoding progress and durable diagnostics`, `feat(history): persist remote compaction output token counts` |
| Compacted-media retention and vacuuming | Efficiency | Remove obsolete inline images and structured tool-output media from compacted model history while retaining bounded, reopenable local-image provenance for one compaction window and providing guarded canonical repair and vacuum paths for existing rollouts. | `codex-rs/core/src/context/compacted_media.rs`<br>`codex-rs/core/src/session/compacted_media_repair.rs`<br>`codex-rs/rollout/src/media_vacuum.rs` | Compaction replacement history, rollout reconstruction and rollback, thread-history projections, plain and compressed rollout storage | `fix(compaction): sanitize retained media and publish canonical history repairs safely` |
| Explicit MCP prompt invocation policy | Capability | Keep configured MCP servers live, deferred, searchable, and callable while hiding selected inventories from direct context until `/mcp use` or `thread/mcpServer/activate` explicitly contributes them. | `codex-rs/core/src/context/mcp_server_use_instructions.rs`<br>`codex-rs/core/src/mcp_tool_exposure.rs`<br>`codex-rs/app-server/src/request_processors/mcp_processor.rs` | MCP catalog/runtime, config editing, session input ordering, compaction, TUI commands, app-server protocol and SDK | `feat(mcp): separate implicit exposure from explicit prompt activation` |
| Cross-home and path-based session forks | Capability | Fork saved or archived history into the active home, copying complete external paginated lineage read-only while preserving ordinary coordinated in-home forks. External UUIDs cannot borrow local runtime or goal authority; flattening cannot manufacture canonical observation evidence. | `codex-rs/cli/src/main.rs`<br>`codex-rs/app-server/src/request_processors/thread_processor.rs`<br>`codex-rs/core/src/thread_manager/external_fork.rs`<br>`codex-rs/thread-store/src/local/fork_copy.rs` | TUI source lookup, app-server `thread/fork`, opened lineage snapshots, rollback/provenance coordinates, runtime/source namespace separation | `feat(cli): fork saved sessions from explicit local sources`, `fix(fork): copy paginated lineage across Codex homes` |
| Thread-scoped shared-server custom instructions | Compatibility | Forward resolved custom base/developer text for new TUI threads, including profile-relative files and CLI overrides, without relabeling catalog instructions, reopening client-local files on the server, mutating daemon defaults, or replacing loaded-thread settings. Terminal guidance remains independently gated. | `codex-rs/tui/src/app_server_session.rs`<br>`codex-rs/core/src/config/mod.rs` | Existing thread/start fields, resolved instruction provenance, per-request config, remote authority, and preserved resume permissions | `fix(tui): forward custom instructions to shared-server thread starts` |
| Durable promoted skills and goal context | Capability | Keep explicitly selected skills discoverable across compaction and resume through canonical receipt publication, reconstruct only current active-goal context, anchor steering to sources authoritative for the objective, prevent ordinary forks/subagents from inheriting goal authority, and tolerate transient user-side Git breakage in configured skill directories. | `codex-rs/ext/goal/src/runtime/objective_projection.rs`<br>`codex-rs/ext/goal/templates/goals/`<br>`codex-rs/ext/skills/src/selection.rs`<br>`codex-rs/core/src/session/durable_context.rs`<br>`codex-rs/core/src/session/checkpoint_publication.rs` | Skills inventory/provider lifecycle, compaction installation, app-server goal/fork APIs, state goal storage, extension contribution ordering | `feat(context): durably publish goal authority and promoted skill inventories`, `fix(goal): preserve objective source authority through steering` |
| Compact discovered skill paths | Efficiency | Preserve canonical skill identity internally while rendering user-facing discovery routes such as `~/.agents/skills/...` instead of resolved checkout paths and repeated home-directory prefixes. | `codex-rs/ext/skills/src/loader/host.rs`<br>`codex-rs/ext/skills/src/provider/host.rs` | Host skill discovery and model-visible inventory rendering | `fix(skills): render compact host discovery paths without changing authority` |
| Thread-scoped skill read reuse | Efficiency | Reuse instructions already read completely in the current thread without a mandatory per-turn reread, while retaining applicability checks, complete first reads, aliases, pagination, and each provider's resource authority. This changes prompt guidance, not caching or discovery. | `codex-rs/ext/skills/src/catalog_prompt.rs` | Host, executor, and Cloud skill catalogs; provider-specific resource access; exact rendered guidance | `fix(skills): avoid rereading skills across turns` |
| Named early skill-read activity | Observability | Attribute `SKILL.md` reads to the selected skill even when the TUI receives the tool call before asynchronous skill metadata has populated ChatWidget state. | `codex-rs/tui/src/chatwidget/skills.rs` | TUI tool-call classification and transcript rendering | `fix(tui): retain skill names in read history before discovery completes` |
| ChatGPT OAuth dictation | Capability | Record editable composer dictation with browser authentication, bounded silence-aware chunking, generation-owned cancellation, and ordered transcript insertion through a pinned route-aware HTTP pool. Preparation and individual uploads have deadlines; there is no claimed whole-recording pipeline deadline. | `codex-rs/tui/src/chatwidget/dictation.rs`<br>`codex-rs/tui/src/dictation/chunk_policy.rs`<br>`codex-rs/tui/src/dictation/transcription.rs` | ChatGPT authentication, shared HTTP routing and Cloudflare-cookie policy, configurable keymaps, composer lifecycle, audio dependencies | `feat(tui): add bounded browser-authenticated editable dictation` |
| Native audio transcript markers | Observability | Keep upstream native model-audio inputs distinct from editable OAuth dictation and render an explicit `[audio]` marker for every attachment in live, paginated, resumed, and exported TUI transcripts instead of silently omitting it. | `codex-rs/tui/src/chatwidget/user_messages.rs`<br>`codex-rs/tui/src/thread_transcript.rs` | App-server user-message projection, pending-steer matching, transcript hydration and export | `fix(tui): retain native audio attachments in transcripts` |
| Configurable shell and unified-exec timing | Capability | Configure initial/background yield windows, requested empty-poll caps, and user-shell command deadlines. An omitted background-poll cap allows long requested waits; output remains bounded and cancellation remains authoritative. `!`/`/shell` commands run as detached background work until exit or explicit cancellation by default, with live IDs exposed through `/ps`, completed through `/stop`, and individually stoppable through `/stop <id>` without blocking model turns or queued follow-ups. | `codex-rs/core/src/tools/handlers/unified_exec/`<br>`codex-rs/core/src/tools/handlers/shell_spec.rs`<br>`codex-rs/core/src/tasks/user_shell.rs`<br>`codex-rs/tui/src/history_cell/exec.rs`<br>`codex-rs/tui/src/chatwidget/slash_dispatch.rs` | Config loading/locking, turn context, tool schemas, process manager, app-server background-terminal control, TUI command lifecycle | `feat(config): make unified exec yield defaults configurable per turn`, `feat(unified-exec): allow uncapped requested background poll windows`, `feat(user-shell): configure command deadlines and preserve timeout output`, `feat(user-shell): add explicit long-running process control` |
| User-shell completion policies | Capability | Let user-authored `!` commands choose passive, completion-wake, or presentation-only result delivery through compact `w` flags; order selected commands behind every earlier user-shell submission without blocking ordinary turns; keep queued commands visible and cancellable through `/ps` and `/stop`; suppress goal idle work while a requested completion wake remains pending; and attribute resulting automatic wake turns distinctly from user-authored work. | `codex-rs/core/src/tasks/user_shell.rs`<br>`codex-rs/core/src/session/user_shell_delivery.rs`<br>`codex-rs/core/src/tasks/user_shell_registration.rs`<br>`codex-rs/core/src/unified_exec/user_shell_queue.rs`<br>`codex-rs/protocol/src/protocol.rs`<br>`codex-rs/app-server-protocol/src/protocol/v2/thread.rs`<br>`codex-rs/tui/src/user_shell_command.rs` | User-shell process lifecycle, active-turn steering and idle wake scheduling, Responses turn-trigger metadata, app-server v2 schema, TUI composer parsing and highlighting | `feat(user-shell): add queued completion policies` |
| Fail-closed human command approval deadlines | Capability | Let unattended on-request command approvals expire against an optional core-authoritative monotonic deadline, reject late responses without executing the command, preserve untimed handling when either app-server timing field is absent, and keep Guardian, patch approvals, and omitted deadlines on their existing timing behavior. | `codex-rs/core/src/session/mod.rs`<br>`codex-rs/core/src/tools/approvals.rs`<br>`codex-rs/app-server-protocol/src/protocol/v2/item.rs`<br>`codex-rs/app-server/src/bespoke_event_handling.rs`<br>`codex-rs/tui/src/bottom_pane/approval_overlay.rs` | Approval protocol and callback routing, delegated turn configuration, app-server/MCP response arbitration, TUI pending-request lifecycle | `feat(approvals): enforce fail-closed human command deadlines` |
| Bounded terminal execution, waits, and configurable previews | Observability | Show which terminal and command an empty poll checked, classify ordinary Python commands as executed rather than directory listings, retain bounded output across initial-yield and process-exit races, with explicit omission and output-close deadline limits, recover offset-proven remote replay gaps, render live wait countdowns, and independently cap agent/tool versus user-shell previews. | `codex-rs/core/src/unified_exec/async_watcher.rs`<br>`codex-rs/core/src/unified_exec/head_tail_buffer.rs`<br>`codex-rs/exec-server/src/client_recovery.rs`<br>`codex-rs/shell-command/src/parse_command.rs`<br>`codex-rs/tui/src/chatwidget/command_lifecycle.rs` | Unified-exec process/output ownership, exec-server replay offsets, completed-process cache, app-server terminal notifications, TUI status/history/pager | `feat(tui): configure source-preserving command output previews`, `feat(tui): show lifecycle-owned advisory wait countdowns`, `fix(history): reconstruct Legacy terminal output without restoring cold runtime state`, `fix(tui): classify non-enumerating Python commands as executed`, `fix(unified-exec): retain bounded output across polling and terminal replay` |
| Conditional mailbox final subscriptions | Capability | Let V1 mailbox sends request a final observation bound only when the accepted message is consumed or advertised by an idle inventory. Recover only current queue-owned tokens with matching graph lifecycle epochs, never from inert historical observations; retain independent explicit observation and wait receipts. | `codex-rs/core/src/agent/control/mailbox_final_subscription.rs`<br>`codex-rs/core/src/session/mailbox_subscription.rs`<br>`codex-rs/state/src/runtime/queued_items/mailbox/final_subscriptions.rs`<br>`codex-rs/state/src/runtime/lifecycle_authority.rs` | Exact runtime ownership, immutable acceptance, graph close/transfer transactions, canonical consumption and final-delivery receipts, and cold recovery | `feat(agents,exec,tui): expose mailbox activity and reliable asynchronous waits` |
| Payload-free mailbox activity | Observability | Show bounded sender/count inventory without message bodies, expose non-consuming mailbox counts for the selected agent, and publish a typed direct-check-mail outcome only after fixed-batch consumption is acknowledged. | `codex-rs/core/src/context/mailbox_inventory_projection.rs`<br>`codex-rs/core/src/session/mailbox_read.rs`<br>`codex-rs/app-server/src/request_processors/thread_mailbox.rs`<br>`codex-rs/tui/src/app/agent_mailbox.rs` | Canonical inventory bounds, request-only projection, app-server experimental read API, immutable selection identity, and TUI asynchronous presentation | `feat(agents,exec,tui): expose mailbox activity and reliable asynchronous waits` |
| Invocation-owned asynchronous terminal waits | Capability | Optionally wait for process exit while letting real user input or invocation cancellation release the wait without killing the process. Show concurrent wait identities, independent clocks, and explicit finished reasons; keep ordinary bounded polling and turn-owned sleep countdowns with actual terminal outcomes or neutral legacy requested-duration history. | `codex-rs/core/src/tools/handlers/unified_exec/write_stdin.rs`<br>`codex-rs/core/src/unified_exec/output_collection.rs`<br>`codex-rs/protocol/src/protocol/terminal_wait.rs`<br>`codex-rs/tui/src/chatwidget/process_wait.rs`<br>`codex-rs/tui/src/chatwidget/sleep.rs` | Pending-versus-transcript output ownership, separate steer activity, cancellation-aware approval/lock waits, protocol adapters, foreground status priority, and detached user-shell lifecycle | `feat(agents,exec,tui): expose mailbox activity and reliable asynchronous waits`, `feat(tui): show owned sleep countdowns and actual completion progress` |
| Configurable diff backgrounds | Capability | Select adaptive, disabled, theme-derived, or custom diff backgrounds while preserving syntax highlighting and normalizing tabs before wrapping. | `codex-rs/tui/src/diff_render.rs`<br>`codex-rs/tui/src/render/highlight.rs` | TUI config/schema and startup theme resolution | `feat(tui): configure diff backgrounds through client-local preferences` |
| Configurable desktop notification previews | Capability | Tune agent-turn, exec-approval, and user-input notification lengths instead of relying on hard-coded grapheme limits. | `codex-rs/tui/src/chatwidget/notifications.rs` | TUI config/schema and notification creation/display paths | `feat(tui): configure notification previews with independent grapheme limits` |
| Scalable transcript switching, review, and paging | Efficiency | Bound terminal scrollback replay when switching agents, render transcript pages (Ctrl+T by default, remappable) in proportion to the visible viewport, and default the pager to a concise chronological review with exact retained output one keypress away. | `codex-rs/tui/src/transcript_reflow.rs`<br>`codex-rs/tui/src/pager_overlay.rs`<br>`codex-rs/tui/src/pager_overlay/transcript.rs`<br>`codex-rs/tui/src/render/renderable.rs` | TUI thread switching, resize/reflow, history-cell representation and navigation, hyperlink mapping | `fix(tui): bound thread-switch scrollback replay with a one-shot row budget`, `feat(tui): add logical viewports and cached rows to static pagers`, `feat(tui): share concise transcript review and canonical target navigation`, `feat(tui): expand agent overview and transcript inspection`, `feat(tui): bound transcript windows and add absolute navigation`, `perf(tui): cache settled command layouts and styled wrapping` |
| Complete tool-review history | Observability | Retain full code-mode invocation/output and approved Guardian assessments through live and replay presentation without removing current footer lifecycle or raw-history behavior. | `codex-rs/tui/src/history_cell/`<br>`codex-rs/tui/src/chatwidget/tool_requests.rs` | Code-mode history, Guardian decisions, replay, owned viewport | `revert(tui): keep complete code mode calls in history`, `revert(tui): keep approved Guardian assessments visible` |
| TLS and Responses startup failure bounds | Platform | Classify nested TLS trust failures without retrying them and bound pre-header Responses startup separately from post-header stream idle handling. | `codex-rs/http-client/`<br>`codex-rs/codex-api/src/endpoint/responses.rs` | Route-aware HTTP fallback, certificate classification, provider idle timeout | `fix(http-client): classify nested TLS failures without retrying trust errors`, `fix(client): bound HTTP response stream startup` |
| Explicit skill catalog budgets | Efficiency | Honor the configured catalog budget without an extra hidden token cap while preserving executor resource authority. | `codex-rs/ext/skills/` | Provider inventory and model-visible skill descriptions | `fix(skills): honor explicit catalog budgets without a hidden token cap` |
| Canonical legacy history migration and shared evidence | Compatibility | Preserve canonical audit history during migration and prevent thread deletion from destroying shared message-board evidence. | `codex-rs/thread-store/src/local/rollout_migration/`<br>`codex-rs/agent-graph-store/` | Exact rollout lineage, migration and deletion ownership | `fix(history): preserve canonical audit replay during legacy migration`, `fix(message-board): preserve shared evidence during thread deletion` |
| On-demand thread naming | Compatibility | Generate an editable suggestion only when the user requests naming or renaming, preserve the originating thread/request and cancellation scope, and persist only a confirmed name. Ordinary prompts no longer trigger hidden naming work. | `codex-rs/tui/src/app/thread_title.rs`<br>`codex-rs/tui/src/app/event_dispatch.rs`<br>`codex-rs/tui/src/chatwidget/interaction.rs` | Structured temporary requests, thread switching, cancellation, text-suggestion identity, manual name persistence | `fix(tui): make thread naming on demand` |
| Graph-preserving singular deletion | Compatibility | Delete only the selected persisted thread, retaining related sessions, incident spawn edges, and durable alias evidence without reviving a tombstoned identity or expanding survivor resume/transfer authority. Explicit bulk deletion remains strict. | `codex-rs/app-server/src/request_processors/thread_delete.rs`<br>`codex-rs/thread-store/src/local/delete_thread.rs`<br>`codex-rs/thread-store/src/in_memory_deletion.rs`<br>`codex-rs/state/src/runtime/thread_deletion.rs` | Selected-writer and lifecycle locks, Paginated reference veto, message-board protection, tombstone transaction, current removal ownership, TUI/CLI confirmation | `fix(delete): preserve related agent threads` |
| Manual verification and release infrastructure | Release | Keep routine verification separate from release packaging while retaining distinct cache namespaces, macOS link/checkpoint recovery, credential-path smoke coverage, and artifact integrity. The routine verification sccache limit is 4G; other jobs retain their own limits. Workflow presence does not assert a passing run for this checkpoint. | `.github/workflows/manual-verify.yml`<br>`.github/workflows/manual-release-build.yml` | Remote build/test/schema jobs, cache identity, release checkpointing, credential isolation, and packaging | `ci(fork): consolidate manual verification and release infrastructure` |

## Integration-specific ownership details

### Build-profile-independent display versions

Ownership anchor: `test(tui): stabilize display versions across build profiles`.
Only test display surfaces use the fixed display value. Production display still aliases the
compiled package version; update eligibility, semantic version comparisons, daemon manifests,
and client/server compatibility checks continue to use the actual runtime version. Current
source-backed clear headers, the optional version footer, startup/SSH fixtures, and status-copy
normalization share the display contract without replacing upstream's current layout.

The historical release owner is partitioned rather than replayed as a 0.156.1 package bump.
Its non-display fixture corrections are integrated into their earlier downstream owners, and its
generated exports require coordinated final-source remote regeneration. Reusable feature commits
retain the source version; the 0.160.0 identity belongs to a separate final version-only commit.
Actual-version snapshots require remote qualification. Snapshot normalization is not an executed
snapshot result, and setting a package version does not produce or validate release binaries.

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
owner90 adds only the new process-ID contract. User-shell queued completion policies remain
with their separate feature owner. Batched formatting is folded into the relevant source owners;
generated contracts and executable validation remain pending.

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

### Absolute transcript navigation and completed-message copy

Ownership anchor: `feat(tui): bound transcript windows and add absolute navigation`.
`app_backtrack.rs` upgrades an in-flight older-history request to a true-beginning jump;
`app/history_pagination.rs` and the existing shared viewport retain continuation and End
cancellation. Fixed inspections do not acquire active-thread mutation or edit authority, and
Find cursor navigation does not request history. The upstream bounded layout caches remain the
only viewport owner; there is no additional historical 256-cell retention layer.
`chatwidget/transcript.rs` keeps ordinary commentary copy separate from the last final answer,
normalizes followup display labels, and retains original source for exact blocks. Current
`KeyEventAction` and the asynchronous clipboard worker keep copy ordering and backend lifetime.
Turn completion still owns trailing timing metadata; retired leading-separator state is not
revived. Constructor and full UI/regression qualification remains remote and unexecuted.

### Settled command layouts and monotonic styled wrapping

Ownership anchor: `perf(tui): cache settled command layouts and styled wrapping`.
`exec_cell/render_cache.rs` retains one width/theme-keyed layout for each rich Review and Full
mode, preserving logical source and hyperlink metadata. Active output and Raw rendering remain
uncached. Command mutations, preview limits, appended reasoning/details, and group joins invalidate
through the cell owner; detached tools and replayed completions cannot bypass that owner.
`wrapping.rs` consumes styled spans monotonically across ascending source slices without replacing
upstream FirstFit, Unicode/control-character guards, or range metadata. Cached static rows use
`terminal_hyperlinks/rows.rs` to keep right-edge wide graphemes in their original row positions.
Parity, invalidation, source identity, and complete Buffer snapshots require remote execution;
source review does not establish timing or memory benchmark improvements.

### Acknowledged live reply-route ownership

Ownership anchor: `feat(multi-agent): add acknowledged live user-controlled reply routes`.
The native controller holds exact endpoint/lifecycle and source-admission ownership while the target
acknowledges its singleton guidance, the source acknowledges relationship snapshots, and a narrow
compare-update activates the live route. Independent workers retain those obligations when a caller
stops waiting. There is no cross-journal atomicity promise: ambiguous effects require canonical
reconciliation, not retries or compensating appends, and inert target guidance can remain.

An explicit disable vetoes new reverse admission, including later model-authored `m`, without
retracting accepted human forward queues or the exact reservation of an accepted reverse wake.
`history/src/agent_reply_route.rs` recognizes only complete, typed, non-client-authored envelopes;
current checkpoint publication retains the accepted source envelope and its metadata rather than
importing authority from a stale replacement. Rollback and migration preserve guidance while forks
filter it, and cold history never reconstructs live permission. V2 retains its native messaging.

The typed action, source audit, two TUI command forms, completion, highlighting, and live-only status
share the existing extracted owners. Current queue result types, startup/writer cleanup, canonical
input order, MCP attribution, and complete checkpoint resume metadata remain intact. The new source
scenarios and inline snapshots still require remote execution and coordinated generated exports.

### Attributed V1 input, task labels, and live messaging policy

Ownership anchor: `feat(multi-agent): add attributed V1 messages and task-path discovery`.
`agent/control/attribution.rs` captures trusted send-time identities and original structured input
separately from the escaped model-visible fragment. Canonical typed input presentation is published
with its source envelopes and persistence receipts; a recipient's agent input never becomes a
human prompt or the receiving exec client's final answer. Main's peer copies and delivery receipts
are live-only presentation, not durable root input or permission. The TUI retains those raw copies
through in-process refresh without replaying a hydrated copy twice or discarding live questions.
Exec ignores TUI-only delivery receipts, including its final-message and output-file fallbacks.

Task labels use shared pure grammar and root-scoped transactional uniqueness. Closed members retain
their labels; ordinary collisions fail. Adoption remaps only imported labels and reports exact
old/new assignments, without inferring ownership from label prefixes. Migration10051 is unchanged
from its original introducing commit. Source-relative TUI selection and opt-in `list_agents` retain
native tree identity, the root's tool gate, pre-pagination filters, and separate send authorization.

Directed overrides and nearest-supervisor subtree defaults are live authority, not historical
instructions. Permission changes publish acknowledged context before compare-installing policy.
Sender-only sampling/compaction snapshots do not mutate permission, restore observers, or deliver
messages. The policy fragment lives in `core/src/context/agent_messaging_policy.rs`; current input
orders, retained-source metadata, MCP attribution, and fail-closed publication remain intact.
A one-use live-revert handoff rekeys only send policy to the same logical thread under lifecycle
and generation checks; cold resume and forks never restore it. Startup retains complete current
`StartThreadOptions`, parent identity, host-controller boundaries, and provisional cleanup.

The existing role-instruction projection remains singular, with helper/reviewer exclusions. The
shared response-flag parser accepts order-independent repetition and pairwise `f`/`x` cancellation.
Global/role messaging defaults and task-label editing remain proposals. New source regressions,
SDK fields, schema generation, and snapshots still require remote qualification; no executable
success is implied. The later mailbox-receipt classifier remains assigned to its defining owner.

### Model-hidden root conclusion oversight

Ownership anchor: `feat(multi-agent): surface unobserved child conclusions in Main`.
`core/src/agent/control/presentation/root_completion_audit.rs` extends the existing immutable
terminal/wait token; it is not a second transcript or model-message queue. Only live native V1
root/child instances in the same runtime and current ownership generation qualify. An exact
final observer takes precedence; absence of a binding is distinct from commentary-only handling.
The current root publisher orders admission, observation claim, history publication, and active
turn selection in that order, releases all gates while awaiting taskless cleanup, and commits the
shared token only after canonical receipt. Generic canonical completion callers retain their
existing contract. Superseded audit claims leave context ownership with the actual observer.

Hidden rows retain `NotVisible` identity, do not replace Main's authored answer, and do not create
model context, an idle wake, a subscription, or a messaging grant. Cold state never recreates this
live obligation. Unknown publication quarantines the exact root without retry. The Legacy and
Paginated request/persistence scenarios and lock-order/recheck regressions are source coverage,
not passed executable validation.

### Receiver-selected mailbox integration

Ownership anchor: `feat(mailbox): add durable receiver-selected input delivery`.
Direct V1 tools carry fixed mailbox operations to the ordered result recorder; nested `check_mail`
is rejected, while nested and hosted-controller waits keep their ordinary completion contract.
Accepted mail, a fixed claim, canonical context, typed presentation, and SQL consumption are separate
boundaries. Recovery reads only receiver-owned canonical history, preserves original envelopes,
and never interprets copied history, payload text, or a readable receipt as a new permission grant.

Fresh user mail retains its user-origin classifications and ordering. Agent mail retains attributed
content kinds for both text and media, so generic rich-input conversion cannot elevate it to user
authorization. Retained-source revisions, MCP attribution, truncation policy, and media preparation
are captured before canonical append and replayed without replacing newer input reservations.
Reserved mailbox response IDs cannot be claimed by ordinary provider/public response publication.
The receiver-owned claim assigns its delivery identity only after generic rich-input preparation
and before retained-source capture; canonical context and presentation must carry that same ID.
Accepted publication workers retain their quarantine guard before first poll as well as after it.

Durable directed/subtree settings are restored only through the native runtime, including inside
armed fork cleanup. An external controller is not silently replaced with native mailbox authority.
The SQL setting is authoritative independently of any later target/source audit publication.

Inventory keeps the complete immutable store snapshot and watermark but bounds fresh model text
to 2,048 bytes and eight sender groups. Larger projections commit to every omitted field through
SHA-256, and recovery still recognizes the original full format without rewriting existing records.
This bound does not truncate selected payloads or change unfiltered consumption. The new direct
dependency reuses the already locked SHA-256 crate version; Bazel lock regeneration remains a
coordinated remote-generation requirement, not a local build or a claimed generated artifact.

Live, replayed, and cold TUI labels accept only confirmed task-path metadata and explicit mapping
clears; absent legacy metadata does not erase current paths. `/mail` retains full drafts and stable
retry identity without silently becoming a steer or queued prompt. Authored regression scenarios
and static Rust parsing are not executable qualification. Later cross-root/name-claim proposals
and final-subscription APIs remain with their separate defining owners.

### Mailbox presentation and available-agent cycling

Ownership anchor: `feat(agents): clarify mailbox outcomes and navigate available agents`.
`check_mail_result` derives terminal `ok`, `empty`, and `rejected` outcomes from an existing
receiver/turn/call claim; it never creates or expands a claim. Acceptance, selected payloads,
durable consumption, and live permissions remain separate. Unknown or hook-rewritten shapes
stay intact, and a claim-store error is not interpreted as an empty inbox.

Canonical inventory context retains the existing typed fragment and durable serializer.
The separate presentation view recognizes full legacy records and the bounded version-2 prefix
without claiming to reconstruct or verify its omitted senders. Only disposable sampling and
supported compaction inputs receive advertised refs and a bounded omission-aware display;
canonical IDs, frontier, commitment, response ordering, and replacement history are unchanged.

Keyboard cycling uses stable root membership and server liveness, with bounded candidate attempts.
Closed or failed attachments remain inspectable through the picker, but are not keyboard targets.
If a later session/profile refresh fails, the candidate receiver is returned and the previous
active channel and draft are restored. Ordinary explicit inspection retains its replay behavior.
Source-authored claim, inventory, compaction, navigation, and channel-recovery regressions still
require remote execution; static parsing does not qualify their runtime behavior.

### Resume-final identity and acknowledged policy-context caching

Ownership anchor: `fix(agents): prevent duplicate finals across delivery, sampling, and resume`.
`core/src/agent/control/resume_delivery.rs` uses strict receiver-owned canonical history before
observation transactions. It filters only an existing completed reconciliation candidate with
the same receiver, child, target turn, and committed response identity. Original rollback
coordinates and contiguous metadata are retained, conflicting duplicate payloads do not prove
delivery, and missing or unreadable history never creates a subscription or suppresses a result.
The current native runtime owns in-flight receipts independently of that historical snapshot.

Messaging-context cache entries install only inside the existing canonical publication
acknowledgment. Failed or ambiguous writes quarantine the session rather than enabling a cache
hit; a cancelled caller cannot discard the owned publication. Permission authority remains
in its existing durable settings owner, and policy context does not request model sampling.
Typed delivery receipt IDs survive commentary conversion while provider-sanitized ordinary IDs
remain ordinary. Resume lifecycle presentation omits old final text and renders Idle without
changing runtime status. Explicit user reservations remain distinct from the later model-only
status-selection API. The latter's premature references are assigned to its defining owner.

### Mailbox activity, conditional finals, and asynchronous waits

Ownership anchor: `feat(agents,exec,tui): expose mailbox activity and reliable asynchronous waits`.
Canonical inventory remains the strict 2048-byte/eight-sender contract. Its request projection
uses receiver-scoped refs/counts without replacing original IDs, source metadata, or durable
inventory evidence. `thread/mailbox/read` is non-consuming; unavailable storage is not an empty
mailbox. A direct `check_mail` outcome is a payload-free canonical item, distinct from the model's
fixed-batch input and from mailbox consumption performed by a wait.

Conditional `zf`/`fz` acceptance atomically stores immutable intent and supersedes older unclaimed
tokens. Consumption or acknowledged inventory binds an exact receiver turn. Current queue tokens
and both graph lifecycle epochs authorize recovery; historical snapshots are inert. Close/transfer
revocations commit with graph ownership before best-effort queue cleanup. Native runtime,
destination, messaging, and observation guards preserve exact actors without loading cold ones.
Final/wait token acknowledgment follows the existing source-preserving canonical publisher, not
an obsolete metadata-free duplicate. Explicit policy replacement preserves newer selections while
retiring only the exact old token.

Acceptance acknowledgement failures use read-only reconciliation, never a second write. An
unsupported operation fails without entering an ambiguous-read loop. Repeated failed reads release
the accepted sender's shutdown receipt when closing, quarantine the unknown outcome, and leave
any stored intent untouched for authoritative reconciliation after reload. Closing observer-binding
retries similarly release runtime ownership without declaring the durable token delivered.

Until-exit waits observe a separate user-steer generation, not arbitrary mailbox arrivals. The
collector drains only pending output and retains the independent bounded transcript. Invocation
cancellation interrupts process-lock, approval, write, and output waits without introducing process
termination. Typed start/finish events retain turn, invocation, command, process, elapsed time, and
completion reason; duplicate or late events cannot take another invocation's state. TUI refresh
preserves Guardian, retry, compaction, and collaboration ownership, reduced-motion scheduling,
asynchronous answer consolidation, and independently running user-shell commands. Sleep history
at this owner records requested duration rather than claiming actual completion time.

Source-authored tests cover authority failures, closing observers, unknown acceptance, immutable
queue intent, fixed consumption, concurrent waits, status priority, and UI replay. They have not run.
Historical Guardian request hashes remain at the preceding baseline until complete final-source
remote regeneration; new and adapted serialized UI snapshots still require executable review.

### Passive finals and active sleep

Ownership anchor: `fix(agents): interrupt active sleep on committed passive finals`.
The session-local watch subscription is installed before the sleep-start event and observes
only fresh publication activity. Native completion consumption and the extracted observed
publisher signal inside the owned callback after canonical acknowledgement and original source
installation, before client presentation can block. Ephemeral publication uses the existing
runtime-only receipt boundary, never a claimed disk receipt or another settlement registry.

Queue-only acceptance does not signal; passive lease consumption can signal once when it actually
installs the model context. Ordinary context, commentary, hidden presentation, explicit wake/defer,
ambiguous writes, and already-settled identities remain separate. The full-envelope receipt owner
survives compaction and keeps retry deduplication distinct from readable model history. No idle
turn is admitted by this signal, and unread mailbox activity retains its independent channel.
The existing metadata, ordering, MCP, lifecycle, and canonical publication owners remain intact.
Legacy/Paginated request scenarios, cancellation, policy, ephemeral, queue-consumption, and
source-envelope regressions are authored coverage, not passed executable qualification.

### Owned sleep progress and completion metadata

Ownership anchor: `feat(tui): show owned sleep countdowns and actual completion progress`.
The sleep handler samples one monotonic elapsed duration for both the terminal event and tool
result, reports Completed/Interrupted/Error, and retains the existing clock-error policy and
passive-final input signal. A dropped invocation has no manufactured terminal outcome.
Extension items and the v2 surface retain optional legacy-compatible completion fields.

Live sleep belongs to the current model turn and item, not the generic busy indicator or active
transcript cell. Stale/replayed starts cannot acquire a timer; foreground approval, retry,
compaction, and collaboration status remain authoritative. Background process redraw does not
overwrite a later owned sleep timer. Matching sleep completion clears only its timer and preserves
newer owners and independent user-shell commands. Authored answer/plan consolidation and notice
FIFO remain unchanged. Partial or absent completion metadata renders requested duration only;
known outcomes render actual elapsed duration rather than claiming the requested interval elapsed.

The pending-input scenario retains its existing non-consuming enqueue helper so an unrelated
list-voices barrier cannot swallow the sleep completion. Public-RPC, persisted event, replay,
partial-metadata, state-priority, and independent-shell regressions remain source coverage pending
remote execution and complete snapshot/schema generation. Earlier full-transcript projection
and premature fixture-field ownership debts remain explicitly assigned to their original owners.

### Batch input admission and root presentation

Ownership anchor: `feat(agents): support batch send_input with shared lifecycle presentation`.
Array targets, including a single-element array, keep their per-recipient result contract;
the scalar target retains its existing output. Shared preparation preserves exact native
ownership, current interruption APIs, captured turn metadata, retained mailbox workers, and
the same scoped-reply and target-queue admission machinery used by scalar input. An error
may follow acceptance, so neither cancellation nor an aggregate failure authorizes resending
already attempted recipients. The current handler abort path remains authoritative.

Typed `batch_id` attribution accompanies each recipient's own canonical input. A normally
completed child batch produces one live-only root mirror keyed by the encoded sender UUID,
originating turn, and call ID; reused or slash-containing IDs cannot alias another copy.
That mirror creates no root context, canonical write, wake, or routing authority. Source
provenance keeps it in the in-process replay buffer across disk refresh while hydrated copies
are suppressed once. Cold history still comes from the sender's canonical lifecycle, not
a fabricated root receipt. Foreign live batch rows wait behind an authored answer; the
sender's own tool lifecycle and pure replay retain their current ordering.

Core, public DTO, legacy conversion, mailbox cancellation, source isolation, and complete
TUI snapshot coverage are retained or authored. Earlier fixture repairs discovered during
integration remain assigned to their original owners for the final history pass. Static
inspection and parsing do not establish executable, schema, or snapshot qualification.

### Captured credential profiles and local runtime identity

Ownership anchor: `feat(auth): select credential profiles while sharing thread storage`.
The selector is captured at startup and threaded through current ConfigManager rebuilds,
cloud/bootstrap policy, login/device/OAuth flows, credential refresh, and logout. Explicit files
do not fall back to another file or a persistent keyring. Process-local external credentials keep
their existing precedence in a selection-scoped ephemeral store. Provider and API-key environment
precedence is unchanged. Selected runtimes bypass only the shared disk model catalog; authoritative
configured catalogs and provider discovery gates remain intact.

Dictation's raw browser preflight and ChatgptAuthSession use the same captured selection. Each
upload revalidates that source and account and captures its account-bound content factory; it never
rediscovers the ambient selector or bypasses network-policy revocation. Missing, malformed, deleted,
non-browser, or changed-account credentials are not invitations to use another profile.

Daemon packages remain shared by home, while PID/settings/update state, control endpoints, and
restart candidate snapshots are profile-scoped. The app-server and updater use one recovery-path
helper, leaving legacy snapshots inert. Startup locks belong to the actual endpoint. Children
inherit the captured absolute selector and backend; default children clear inherited selectors.
Offline lifecycle lookup does not require fresh authentication and rejects ambiguous backend
matches. Actual local connections verify both initialized home and full opaque profile identity;
socket discovery and display labels are not proof. Profile response timeouts cover the entire
correlated exchange, not each unrelated message.

Current startup composers and executor restrictions are preserved. Delayed profile discovery in
interactive and archive/queue paths reuses the existing launch-selection gate, so it cannot override
workload identity or an explicit executor. Normal ephemeral TUI sessions remain embedded. The
explicit agents overview retains required shared-server startup using the captured profile and
cannot silently become an empty embedded overview. Optional ordinary startup can fall back;
required startup and reconnect cannot. Remote WebSocket hosts retain their own credential authority,
and a local selector is rejected rather than forwarded as a foreign-host path.

Windows private-directory, non-elevated-peer, launch restriction, and PID-fingerprint protections
remain in place; the elevated provisioning service retains its upstream auth boundary. Default
library wrappers and the intentionally default-auth sample do not rediscover process environment.
Source tests cover selected browser credentials, recovery isolation, actual-connection mismatch,
unrelated response IDs/deadlines, required attachment, and discovered-socket launch exclusions.
Schema exports, complete status snapshots, Cargo/Bazel lock reconciliation, cross-platform tests,
and executable qualification remain remote work, not a passing result of this source rebase.

### Durable shutdown and owning-subtree unload

Ownership anchor: `feat(lifecycle): durably unload owning subtrees on graceful client exit`.
`thread/unload` captures the owning spawn root and loaded descendants, not ordinary history-fork
ancestry. Subscriber preflight precedes sealing. Lifecycle locks are released before accepted
completion drains and reacquired for exact-instance removal; a stopped facade, cancellation
request, output-close signal, or successful kill RPC does not acknowledge writer release.
Failures retain all captured entries for retry, including stopped actors and pending cancelled-spawn
alias cleanup. Sealed descendants prevent cold recreation of an already-unloaded ancestor.

The existing provisional spawn guard owns graph publication and input uncertainty. Prepared
receipts remain armed until consumed, not merely sent. Definite abandoned setup may complete
its own alias rollback, but unknown input or graph acknowledgement retains its original evidence.
Managed resume and fork startup keep captured options, host-controller choice, credential profile,
persistence acquisition, and the exact actor owner through cancellation and durable cleanup.
History forks activate the saved alias-reservation implementation here, without treating copied
external UUIDs as authority over local runtime or alias state.

The shell's legacy tracker and aggregate durable barrier observe the same accepted producer.
Even an unpolled shell execution registers its queue-cleanup receipt before yielding. Local and
remote exit-confirmation guards are retained before first poll. Pending-output collection remains
separate from canonical bounded transcript retention; final output and network-denial watchers
complete before inventory retirement. Code Mode retains provider openings, generations, callbacks,
and unknown close outcomes, and Guardian retains accepted creation and exact reviewer actors.
These owners drain before final history, metadata, and writer shutdown; a failed phase stays
retryable rather than falling back to unacknowledged legacy teardown.

Graceful TUI exit unloads the owning root even when viewing a child. A refused unload keeps the
composer and parked App-owned voice intact. Explicit `/disconnect` is available only for shared
servers; once exit is accepted, client-owned dictation, voice, and dynamic tasks are retired through
their actual owners. Existing unsubscribe/navigation and interrupt-first behavior remain separate.
Source, RPC, failure-injection, cancellation, and snapshot coverage require remote execution and
coordinated schema generation; no local executable qualification is claimed.

## Integration boundaries and deferred work

This checkpoint records integrated source ownership, not executable qualification. Coordinated
schema generation, snapshot reconciliation, remote tests, and release packaging remain outstanding.
Durable receiver-selected send settings, acknowledged live reply routes, and exact-turn response
subscriptions are separate policy surfaces: restoring one must not silently recreate another.
Likewise, durable model-hidden root conclusions are independent of best-effort nonpersistent peer
receipts; a cold root transcript cannot infer those receipts from current graph membership.

Shared identity presentation is owned by `feat(tui): unify agent identity styling and fresh spawn references`.
`tui/src/multi_agents/identity_header.rs` renders nicknames, roles, task paths, trusted numeric refs,
and partial model/reasoning metadata consistently across input, lifecycle, and completion rows.
`tui/src/agent_color.rs` uses terminal-adapted decorative hues without treating color as identity.
Core presentation refs and fresh-spawn events carry canonical alias refs when available; absent
legacy refs are not inferred from labels. `tui/src/chatwidget/collab_metadata.rs` and
`tui/src/thread_transcript/agent_metadata.rs` preserve confirmed refs across partial history,
while only successful user-control mappings can replace them or authoritatively clear task paths.
Canonical UUIDs, raw audit output, and runtime authority remain separate from these labels.
Source-level and macro-constructor inspections do not replace final schema and executable qualification.

The overview and fixed transcript inspection are owned by
`feat(tui): expand agent overview and transcript inspection`. The entrypoints are
`tui/src/app/agent_control_pane.rs`, `tui/src/app/agent_transcript_inspection.rs`,
`tui/src/pager_overlay/transcript.rs`, and `tui/src/chatwidget/collab_metadata.rs`.
Fill-available lists are opt-in and page by rendered items; ordinary pickers keep their caps.
The selected agent's transcript remains read-only and does not acquire the active thread's live
tail, prompt-edit authority, or later consolidation. Shared keymap conflicts, copy preference,
typed secondary actions, paginated live-item reconciliation, and surviving child voice ownership
remain with their current owners. A transcript binding that consumes Tab does not advertise an
unreachable Tab control action. Partial model/effort hints never invent omitted settings;
failed or indeterminate control audit does not replace confirmed live, replayed, or cold labels.
Countdown zero is still an estimate, not a completion receipt. Source and authored regressions
require remote qualification, including complete inspection/layout snapshot generation.

- Upstream unified exec supersedes the legacy `shell`/`shell_command` handlers and the fork's `feat(config): add default shell command timeout` carry. `exec_command_timeout_ms` is retired, not a missing feature to replay or an alias for a yield window or user-shell deadline. Normal resumable `exec_command` yields output without terminating the process; the managed-policy one-shot fallback retains its separate per-call deadline. Keep the maintained yield-window, poll-cap, and human user-shell timing capabilities above. Historical commits and backup refs remain valid evidence; remove the old setting from user configs after promotion rather than restoring the handlers or rewriting historical backups.
- The pinned target already supplies the current realtime voice/WebRTC owners. Historical restoration and Linux-audio patches were not transplanted wholesale; editable browser-authenticated dictation is a separate downstream capability.
- Preserve the current Paginated defaults for new durable TUI/exec sessions and explicit/stored Legacy compatibility. Historical default-switch subjects in the replay stack do not override the integrated runtime contract.
- Stable and experimental app-server schemas/bundles, configuration descriptions, persisted-history/embedded Python SDK artifacts, and Bazel dependency locks require coordinated final-source remote regeneration. Historical generated artifacts are not evidence that the current source contracts have been validated.
- Canonical publication ambiguity quarantines the exact session. Readable history is not acknowledgment, accepted receipts cannot be retargeted, and process-local ownership fences do not promise crash-atomic transfers or fsync durability.
- The model-tool terminal-wake and workspace-root AGENTS documents remain proposals. Explicit settled close-response replay, scoped replies, and target-owned queue input are integrated under their existing owners. Automatic retry, crash-atomic replay, and generalized close-replay guarantees are not implied; these features do not implement model-terminal process-exit wake scheduling.
- Per-owner source reviews, source repairs, qualification-tail relocations, and the single batched formatting result are recorded and folded into their downstream owners. No local compilation, tests, or compile-dependent generation were performed. Previously built 0.156.1 binaries are not qualification of this source.
- The 0.160.0 release identity is isolated in the final version-only owner, not imported through the excluded upstream release commit. Packaging still requires remotely verified binaries, native components, exact producer identities, and reviewed generated contracts; a versioned source tree is not a published release.

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
