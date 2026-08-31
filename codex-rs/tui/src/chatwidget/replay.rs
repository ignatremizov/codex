//! Thread replay rendering for `ChatWidget`.
//!
//! This module rehydrates turns and items into transcript state while avoiding
//! live-only side effects. An in-progress snapshot does not carry reasoning completion
//! state: keep its trailing reasoning provisional until live events identify the next item.

use super::*;

impl ChatWidget {
    /// Restores the active typed reasoning item after a thread switch or session refresh.
    /// Its completion carries the full summary if the bounded event buffer lost earlier deltas.
    pub(crate) fn restore_active_reasoning_item(
        &mut self,
        started: codex_app_server_protocol::ItemStartedNotification,
        parts: Option<(Vec<String>, Vec<String>)>,
    ) {
        let turn_id = started.turn_id;
        let id = started.item.id().to_string();
        self.realtime_conversation
            .agent_items
            .entry((turn_id, id.clone()))
            .or_insert(realtime::RealtimeAgentItemOrigin::Typed);
        self.on_reasoning_item_started(id);
        self.status_state.reasoning_recovered_after_refresh = true;
        if let Some((summary, content)) = parts {
            let reasoning_parts = summary.into_iter().chain(
                self.config
                    .show_raw_agent_reasoning
                    .then_some(content)
                    .into_iter()
                    .flatten(),
            );
            for (index, delta) in reasoning_parts.enumerate() {
                if index > 0 {
                    self.on_reasoning_section_break();
                }
                self.on_agent_reasoning_delta(delta);
            }
        }
    }

    /// The first reasoning update after resume can arrive without an item/started event.
    /// Returns false for unrelated reasoning while waiting for the resumed turn's first update.
    pub(super) fn recover_resumed_reasoning(&mut self, notification: &ServerNotification) -> bool {
        let Some(resumed_turn_id) = self.status_state.reasoning_resume_turn_id.clone() else {
            return true;
        };
        let (turn_id, item_id) = match notification {
            ServerNotification::ReasoningSummaryTextDelta(delta) => {
                (&delta.turn_id, &delta.item_id)
            }
            ServerNotification::ReasoningTextDelta(delta) => (&delta.turn_id, &delta.item_id),
            ServerNotification::ReasoningSummaryPartAdded(part) => (&part.turn_id, &part.item_id),
            ServerNotification::ItemCompleted(
                codex_app_server_protocol::ItemCompletedNotification {
                    turn_id,
                    item: ThreadItem::Reasoning { id, .. },
                    ..
                },
            ) => (turn_id, id),
            ServerNotification::ItemStarted(item)
                if resumed_turn_id == item.turn_id
                    && !matches!(item.item, ThreadItem::UserMessage { .. })
                    && self.status_state.reasoning_item_id.as_deref() != Some(item.item.id()) =>
            {
                // A new item closes a trailing snapshot that was already complete at resume.
                self.on_agent_reasoning_final();
                return true;
            }
            _ => return true,
        };
        if resumed_turn_id != *turn_id
            || self.is_realtime_delegated_reasoning_item(turn_id, item_id)
        {
            return false;
        }
        self.restore_active_reasoning_item(
            codex_app_server_protocol::ItemStartedNotification {
                deadline_at_ms: None,
                thread_id: self.thread_id.map(|id| id.to_string()).unwrap_or_default(),
                turn_id: turn_id.clone(),
                item: ThreadItem::Reasoning {
                    id: item_id.clone(),
                    summary: Vec::new(),
                    content: Vec::new(),
                },
                started_at_ms: 0,
            },
            /*parts*/ None,
        );
        true
    }

    /// Flush prior activity before live or replayed assistant text.
    pub(super) fn prepare_assistant_message(&mut self) {
        self.flush_unified_exec_wait_streak();
        self.flush_active_cell();
    }

    /// Replay a subset of initial events into the UI to seed the transcript when
    /// resuming an existing session. This approximates the live event flow and
    /// is intentionally conservative: only safe-to-replay items are rendered to
    /// avoid triggering side effects. Event ids are passed as `None` to
    /// distinguish replayed events from live ones.
    pub(crate) fn replay_thread_turns(&mut self, turns: Vec<Turn>, replay_kind: ReplayKind) {
        if !turns.is_empty() || matches!(replay_kind, ReplayKind::ThreadSnapshot) {
            self.bottom_pane.dismiss_composer_sparkle();
        }
        if matches!(replay_kind, ReplayKind::ThreadSnapshot) && !turns.is_empty() {
            self.warning_display_state.startup_complete = true;
        }
        let latest_turn_id = turns.last().map(|turn| turn.id.clone());
        let hidden_nested_review_turns = std::iter::once(/*value*/ false)
            .chain(turns.windows(/*size*/ 2).map(|turns| {
                crate::app_backtrack::is_hidden_nested_review_turn(&turns[0], &turns[1])
            }))
            .collect::<Vec<_>>();
        for (turn, hidden_nested_review_turn) in turns.into_iter().zip(hidden_nested_review_turns) {
            self.restore_realtime_transcripts_before_turn(&turn.id);
            // Defer completed metadata-only turns until their page loads. Active
            // turns must restore their lifecycle even before any items are available.
            if turn.status == TurnStatus::Completed
                && turn.items_view == codex_app_server_protocol::TurnItemsView::NotLoaded
                && turn.items.is_empty()
            {
                continue;
            }
            let Turn {
                id: turn_id,
                items_view: _,
                items,
                status,
                mut error,
                started_at,
                completed_at,
                duration_ms,
            } = turn;
            let turn_replay_kind =
                if replay_kind.preserves_live_processes() && status != TurnStatus::InProgress {
                    ReplayKind::ReplayOnlyThreadSnapshot
                } else {
                    replay_kind
                };
            let delegated = items.iter().any(|item| {
                matches!(item, ThreadItem::UserMessage { content, .. }
                    if realtime::realtime_delegation_input(content).is_some())
            });
            if matches!(status, TurnStatus::InProgress) {
                if delegated {
                    self.remember_realtime_delegated_reasoning_turn(&turn_id);
                }
                self.warning_display_state.startup_complete = true;
                self.turn_lifecycle.last_turn_id = Some(turn_id.clone());
                self.last_non_retry_error = None;
                // Initial resume retains its existing task/reasoning lifecycle independently
                // of whether historical terminal processes can be restored.
                if replay_kind != ReplayKind::ReplayOnlyThreadSnapshot {
                    self.on_task_started();
                }
            }
            let trailing_reasoning_id = (status == TurnStatus::InProgress)
                .then(|| items.last())
                .flatten()
                .and_then(|item| match item {
                    ThreadItem::Reasoning { id, .. } => Some(id.clone()),
                    _ => None,
                });
            let mut replaying_delegation = false;
            for item in items {
                if matches!(&item, ThreadItem::UserMessage { content, .. }
                    if realtime::realtime_delegation_input(content).is_some())
                {
                    replaying_delegation = true;
                }
                // Voice can steer a typed turn already in progress. Its earlier
                // commentary and reasoning still belong to the typed request.
                if replaying_delegation && realtime::is_private_realtime_agent_item(&item) {
                    continue;
                }
                if hidden_nested_review_turn && matches!(item, ThreadItem::UserMessage { .. }) {
                    continue;
                }
                if trailing_reasoning_id.as_deref() == Some(item.id())
                    && let ThreadItem::Reasoning {
                        id,
                        summary,
                        content,
                    } = item
                {
                    self.restore_active_reasoning_item(
                        codex_app_server_protocol::ItemStartedNotification {
                            deadline_at_ms: None,
                            thread_id: self.thread_id.map(|id| id.to_string()).unwrap_or_default(),
                            turn_id: turn_id.clone(),
                            item: ThreadItem::Reasoning {
                                id,
                                summary: Vec::new(),
                                content: Vec::new(),
                            },
                            started_at_ms: 0,
                        },
                        Some((summary, content)),
                    );
                } else {
                    self.replay_thread_item(item, turn_id.clone(), turn_replay_kind);
                }
            }
            let status = if hidden_nested_review_turn {
                TurnStatus::Completed
            } else {
                status
            };
            if status == TurnStatus::InProgress {
                self.status_state.reasoning_resume_turn_id = Some(turn_id.clone());
            }
            // A resolved historical precaution must not clear the restored draft or input queue.
            if Some(&turn_id) != latest_turn_id.as_ref()
                && error.as_ref().is_some_and(|error| {
                    error.codex_error_info
                        == Some(AppServerCodexErrorInfo::MisalignmentPolicyViolation)
                })
            {
                error = None;
            }
            if hidden_nested_review_turn {
                self.turn_lifecycle
                    .rendered_completion_turn_ids
                    .insert(turn_id.clone());
            }
            if matches!(
                status,
                TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed
            ) {
                self.handle_turn_completed_notification(
                    TurnCompletedNotification {
                        thread_id: self.thread_id.map(|id| id.to_string()).unwrap_or_default(),
                        turn: Turn {
                            id: turn_id,
                            items_view: codex_app_server_protocol::TurnItemsView::NotLoaded,
                            items: Vec::new(),
                            status,
                            error,
                            started_at,
                            completed_at,
                            duration_ms,
                        },
                    },
                    Some(turn_replay_kind),
                );
            }
            if !turn_replay_kind.preserves_live_processes() {
                self.finalize_replayed_process_tracking();
            }
        }
        if !replay_kind.preserves_live_processes() {
            // Process-local terminals cannot survive a cold or replay-only boundary. Persisted
            // in-progress command items remain useful audit history, but must not repopulate
            // `/ps` or the live footer. A cold resume may subsequently receive genuinely live
            // buffered events; replay-only callers finalize once more after their event buffer.
            self.finalize_replayed_process_tracking();
        }
    }

    pub(crate) fn replay_thread_item(
        &mut self,
        item: ThreadItem,
        turn_id: String,
        replay_kind: ReplayKind,
    ) {
        self.handle_thread_item(
            item,
            turn_id,
            ThreadItemRenderSource::ReplayedTurnItem(replay_kind),
        );
    }

    pub(super) fn handle_thread_item(
        &mut self,
        item: ThreadItem,
        turn_id: String,
        render_source: ThreadItemRenderSource,
    ) {
        if inter_agent_transcript::is_inter_agent_message(&item) {
            self.on_inter_agent_message(item);
            return;
        }
        let from_replay = render_source.is_replay();
        let replay_kind = render_source.replay_kind();
        match item {
            ThreadItem::UserMessage {
                id,
                content,
                client_id,
            } => {
                if let Some(replies) = crate::async_question_reply::parse_input(&content) {
                    let ids = replies
                        .into_iter()
                        .map(|reply| reply.question_item_id)
                        .collect::<Vec<_>>();
                    self.bottom_pane.question_editor().resolve_answers(&ids);
                    self.refresh_pending_input_preview();
                    self.request_redraw();
                }
                self.on_committed_user_message(
                    &content,
                    client_id.as_deref(),
                    from_replay,
                    &turn_id,
                    &id,
                );
            }
            ThreadItem::AgentMessage {
                id,
                text,
                phase,
                memory_citation,
                delivery,
                questions,
                ..
            } => {
                let cell = multi_agents::background_completion_history_cell_from_agent_message(
                    &id,
                    &text,
                    phase.as_ref(),
                    self.local_settings.tui.agent_response_preview_lines,
                    |thread_id| self.collab_agent_metadata(thread_id),
                )
                .or_else(|| {
                    if !from_replay {
                        return None;
                    }
                    multi_agents::background_commentary_history_cell_from_agent_message(
                        &id,
                        &text,
                        phase.as_ref(),
                        self.local_settings.tui.agent_response_preview_lines,
                        |thread_id| self.collab_agent_metadata(thread_id),
                    )
                });
                if let Some(cell) = cell {
                    self.on_collab_event(cell);
                    return;
                }
                if self.complete_realtime_delegated_agent_item(
                    &turn_id,
                    &ThreadItem::AgentMessage {
                        inter_agent_source: None,
                        id: id.clone(),
                        text: text.clone(),
                        phase: phase.clone(),
                        memory_citation: memory_citation.clone(),
                        delivery,
                        questions: questions.clone(),
                    },
                    from_replay,
                ) {
                    return;
                }
                self.on_agent_message_item_completed(
                    AgentMessageItem {
                        id,
                        content: vec![AgentMessageContent::Text { text }],
                        phase,
                        memory_citation: memory_citation.map(|citation| {
                            codex_protocol::memory_citation::MemoryCitation {
                                entries: citation
                                    .entries
                                    .into_iter()
                                    .map(|entry| {
                                        codex_protocol::memory_citation::MemoryCitationEntry {
                                            path: entry.path,
                                            line_start: entry.line_start,
                                            line_end: entry.line_end,
                                            note: entry.note,
                                        }
                                    })
                                    .collect(),
                                rollout_ids: citation.thread_ids,
                            }
                        }),
                        delivery,
                        questions,
                        sub_agent_completion: None,
                    },
                    &turn_id,
                    from_replay,
                );
            }
            ThreadItem::Plan { text, .. } => self.on_plan_item_completed(text),
            ThreadItem::Reasoning {
                id,
                summary,
                content,
            } => {
                let recover_completion = self.status_state.reasoning_recovered_after_refresh
                    && (!summary.is_empty()
                        || (self.config.show_raw_agent_reasoning && !content.is_empty()));
                if from_replay || recover_completion {
                    if from_replay {
                        self.on_reasoning_item_started(id);
                    } else {
                        // A refreshed snapshot can omit the active item and its earlier deltas.
                        // Reconcile with the complete item before committing the transcript.
                        self.reasoning_buffer.clear();
                        self.reasoning_summary_parts.clear();
                    }
                    let reasoning_parts = summary.into_iter().chain(
                        self.config
                            .show_raw_agent_reasoning
                            .then_some(content)
                            .into_iter()
                            .flatten(),
                    );
                    for (index, delta) in reasoning_parts.enumerate() {
                        if index > 0 {
                            self.on_reasoning_section_break();
                        }
                        self.on_agent_reasoning_delta(delta);
                    }
                }
                self.on_agent_reasoning_final();
            }
            item @ ThreadItem::CommandExecution {
                status: codex_app_server_protocol::CommandExecutionStatus::InProgress,
                ..
            } if from_replay
                && replay_kind
                    .is_some_and(|replay_kind| !replay_kind.preserves_live_processes()) =>
            {
                // A replay-only command cannot still own a process. Materialize the saved start
                // as an active cell so the replay boundary can finalize it into audit history.
                self.handle_command_execution_started_now(item);
            }
            item @ ThreadItem::CommandExecution {
                status: codex_app_server_protocol::CommandExecutionStatus::InProgress,
                ..
            } => self.on_command_execution_started(item, /*deadline_at_ms*/ None, &turn_id),
            item @ ThreadItem::CommandExecution {
                source: ExecCommandSource::Agent | ExecCommandSource::UnifiedExecStartup,
                status:
                    codex_app_server_protocol::CommandExecutionStatus::Completed
                    | codex_app_server_protocol::CommandExecutionStatus::Failed,
                ..
            } if from_replay => self.handle_command_execution_completed_now(item),
            item @ ThreadItem::CommandExecution { .. } => {
                self.on_command_execution_completed(item, &turn_id)
            }
            ThreadItem::FileChange {
                changes,
                status: codex_app_server_protocol::PatchApplyStatus::InProgress,
                ..
            } => {
                if render_source.reconstructs_file_change() && !changes.is_empty() {
                    self.on_patch_apply_begin(file_update_changes_to_display(changes));
                }
            }
            ThreadItem::FileChange {
                id,
                changes,
                status,
            } => {
                if render_source.reconstructs_file_change()
                    && status == codex_app_server_protocol::PatchApplyStatus::Completed
                    && !changes.is_empty()
                {
                    self.on_patch_apply_begin(file_update_changes_to_display(changes.clone()));
                }
                self.on_file_change_completed(ThreadItem::FileChange {
                    id,
                    changes,
                    status,
                });
            }
            item @ ThreadItem::McpToolCall {
                status: codex_app_server_protocol::McpToolCallStatus::InProgress,
                ..
            } => self.on_mcp_tool_call_started(item),
            item @ ThreadItem::McpToolCall { .. } => self.on_mcp_tool_call_completed(item),
            ThreadItem::WebSearch(item) => {
                self.on_web_search_begin(item.id.clone());
                self.on_web_search_end(
                    item.id,
                    item.query,
                    item.action
                        .unwrap_or(codex_app_server_protocol::WebSearchAction::Other),
                );
            }
            ThreadItem::ImageView { id: _, path } => {
                self.on_view_image_tool_call(path);
            }
            ThreadItem::ImageGeneration(item) => {
                self.on_image_generation_end(
                    item.id,
                    item.status,
                    item.revised_prompt,
                    item.saved_path,
                );
            }
            ThreadItem::EnteredReviewMode { review, .. } => {
                if from_replay {
                    self.enter_review_mode_with_hint(review, /*from_replay*/ true);
                }
            }
            ThreadItem::ExitedReviewMode { .. } => {
                self.exit_review_mode_after_item();
            }
            ThreadItem::ContextCompaction {
                id,
                summary,
                message,
                decode_error,
                available_skills,
                ..
            } => {
                self.on_context_compaction_completed(
                    &id,
                    &turn_id,
                    from_replay,
                    summary,
                    message,
                    decode_error,
                );
                if !available_skills.is_empty() {
                    self.add_to_history(history_cell::new_info_event(
                        format!(
                            "Available skills after compaction: {}",
                            available_skills.join(", ")
                        ),
                        /*hint*/ None,
                    ));
                }
            }
            ThreadItem::FunctionCallOutput {
                name,
                namespace,
                output,
                ..
            } => {
                if let Some((source_thread_id, prompt)) =
                    crate::dynamic_tools::parse_delegated_tool_output(
                        &name,
                        namespace.as_deref(),
                        &output,
                    )
                {
                    self.add_to_history(history_cell::PrefixedWrappedHistoryCell::new(
                        format!("Sent by Codex from task {source_thread_id}\n{prompt}"),
                        "• ".dim(),
                        "  ",
                    ));
                }
            }
            ThreadItem::HookPrompt { .. } => {}
            ThreadItem::CollabAgentToolCall {
                id,
                tool,
                status,
                observe_commentary,
                wake_on_completion,
                target_messages,
                queue_input,
                sender_thread_id,
                receiver_thread_ids,
                receiver_agents,
                prompt,
                model,
                reasoning_effort,
                agents_states,
            } => self.on_collab_agent_tool_call(
                ThreadItem::CollabAgentToolCall {
                    id,
                    tool,
                    status,
                    observe_commentary,
                    wake_on_completion,
                    target_messages,
                    queue_input,
                    sender_thread_id,
                    receiver_thread_ids,
                    receiver_agents,
                    prompt,
                    model,
                    reasoning_effort,
                    agents_states,
                },
                /*deadline_at_ms*/ None,
                &turn_id,
            ),
            item @ ThreadItem::SubAgentActivity { .. } => self.on_sub_agent_activity(item),
            item @ ThreadItem::DynamicToolCall { .. } => self.on_dynamic_tool_item(item),
            item @ ThreadItem::UserAgentControl { .. } => {
                self.remember_user_agent_control_metadata(&item);
                if let Some(cell) = crate::history_cell::new_user_agent_control(item) {
                    self.add_boxed_history(Box::new(cell));
                }
            }
            ThreadItem::Sleep(_) => {}
        }

        if matches!(
            replay_kind,
            Some(ReplayKind::ThreadSnapshot | ReplayKind::ReplayOnlyThreadSnapshot)
        ) && turn_id.is_empty()
        {
            self.request_redraw();
        }
    }
}
