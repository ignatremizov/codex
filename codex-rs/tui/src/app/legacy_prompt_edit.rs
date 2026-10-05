//! Legacy prompt editing uses one guarded mutation and an ordered history-reset fence.
//! Failure after issuance leaves the affected thread read-only for this App's lifetime.

use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::app_server_event_targets::server_request_thread_id;
use super::*;
use crate::app_backtrack::LegacyRollbackTarget;
use crate::app_server_session::LegacyRollbackOutcome;
use crate::chatwidget::UserMessage;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadHistoryMode;
use color_eyre::eyre::bail;
use color_eyre::eyre::eyre;

impl App {
    /// Reject queued mutations before they can alter the preserved composer or start an RPC.
    pub(super) fn history_recovery_blocks_event(&self, event: &AppEvent) -> bool {
        let target = match event {
            AppEvent::RevertSessionForPromptEdit { thread_id, .. }
            | AppEvent::SubmitThreadOp { thread_id, .. }
            | AppEvent::SetThreadGoalDraft { thread_id, .. }
            | AppEvent::SetThreadGoalStatus { thread_id, .. }
            | AppEvent::ClearThreadGoal { thread_id }
            | AppEvent::RetrySafetyBufferedTurn { thread_id, .. }
            | AppEvent::ConfirmSafetyBufferedRetry { thread_id, .. }
            | AppEvent::SyncThreadGitBranch { thread_id, .. }
            | AppEvent::RenameAgentsOverviewThread { thread_id, .. }
            | AppEvent::GeneratedThreadTitle { thread_id, .. }
            | AppEvent::StopAgentsOverviewThread { thread_id }
            | AppEvent::RunAgentsOverviewAction { thread_id, .. }
            | AppEvent::ApplyPermissionShortcut { thread_id, .. }
            | AppEvent::UpdateLunaReserveReasoning { thread_id, .. }
            | AppEvent::ApplyBackendBannerFallback { thread_id }
            | AppEvent::ApproveRecentAutoReviewDenial { thread_id, .. } => Some(*thread_id),
            AppEvent::ContinueMisalignment(review) => Some(review.thread_id),
            AppEvent::CodexOp(_)
            | AppEvent::ForkCurrentSession { .. }
            | AppEvent::StartSide { .. }
            | AppEvent::StartManagedWorktree {
                mode: crate::app_event::ManagedWorktreeMode::Fork,
                ..
            }
            | AppEvent::ArchiveCurrentThread
            | AppEvent::DeleteCurrentThread
            | AppEvent::UpdateModel(_)
            | AppEvent::UpdateReasoningEffort(_)
            | AppEvent::UpdatePlanModeReasoningEffort(_)
            | AppEvent::ApplyAdvancedReasoning { .. }
            | AppEvent::UpdateAskForApprovalPolicy(_)
            | AppEvent::UpdateActivePermissionProfile(_)
            | AppEvent::SelectPermissionProfile(_)
            | AppEvent::UpdateApprovalsReviewer(_) => {
                self.active_thread_id.or(self.chat_widget.thread_id())
            }
            _ => None,
        };
        target.is_some_and(|id| self.history_recovery_required.contains(&id))
    }

    pub(super) async fn edit_legacy_prompt(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        mut thread: Thread,
        selected_cell: Arc<dyn HistoryCell>,
        nth_user_message: usize,
        mut prompt: UserMessage,
    ) -> Result<()> {
        let thread_id = ThreadId::from_string(&thread.id)?;
        let selection: Result<usize> = async {
            let channel = self
                .thread_event_channels
                .get(&thread_id)
                .ok_or_else(|| eyre!("the selected thread is no longer available"))?;
            let (cached, start_item, latest_turn_id) = {
                let store = channel.store.lock().await;
                (
                    thread_turn_materialization::materialized_thread_turns(&store),
                    store.turns.iter().find_map(|turn| {
                        turn.items
                            .first()
                            .map(|item| (turn.id.clone(), item.id().to_string()))
                    }),
                    store.latest_turn_id.clone(),
                )
            };
            thread.turns = match cached {
                Some(turns) => turns,
                None => {
                    let canonical = app_server
                        .thread_read(thread_id, /*include_turns*/ true)
                        .await?;
                    if canonical.history_mode != ThreadHistoryMode::Legacy {
                        bail!("thread history changed; reload the session before editing");
                    }
                    let mut store = channel.store.lock().await;
                    // Certify only the exact loaded snapshot, never a clipped window.
                    store.turn_history_complete = store.turns == canonical.turns;
                    canonical.turns
                }
            };
            if thread.turns.last().map(|turn| &turn.id) != latest_turn_id.as_ref() {
                bail!("thread history changed; reload the session before editing this prompt");
            }
            crate::app_backtrack::selected_prompt_turn_index(
                &thread.turns,
                selected_cell
                    .as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .and_then(|cell| cell.identity.get()),
                start_item.as_ref(),
                nth_user_message,
                &mut prompt,
            )
        }
        .await;
        let selected = match selection {
            Ok(selected) => selected,
            Err(err) => {
                self.restore_backtrack_prompt_after_revert_error(prompt, err);
                tui.frame_requester().schedule_frame();
                return Ok(());
            }
        };
        if self.config.features.enabled(Feature::ForkPromptEdits) {
            self.fork_for_prompt_edit(
                tui,
                app_server,
                thread_id,
                thread.turns[selected].id.clone(),
                prompt,
            )
            .await;
            return Ok(());
        }
        let target = LegacyRollbackTarget::new(&thread.turns, selected)?;
        self.chat_widget
            .restore_user_message_to_composer(prompt.clone());
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        self.pending_thread_switch_resets += 1;
        self.history_recovery_required.insert(thread_id);
        let (request_id, outcome) = app_server.rollback_legacy_thread(thread_id, target).await;
        let response = match outcome {
            LegacyRollbackOutcome::Rejected(err) => {
                self.history_recovery_required.remove(&thread_id);
                self.pending_thread_switch_resets -= 1;
                self.chat_widget
                    .set_queue_autosend_suppressed(/*suppressed*/ false);
                self.restore_backtrack_prompt_after_revert_error(prompt, err);
                tui.frame_requester().schedule_frame();
                return Ok(());
            }
            LegacyRollbackOutcome::Unknown => {
                bail!(
                    "prompt edit outcome is unknown; your draft is preserved. Reopen this session to reload its history before continuing"
                );
            }
            LegacyRollbackOutcome::Refreshed(response) => Some(response.thread),
            LegacyRollbackOutcome::CommittedRefreshRequired => None,
        };
        self.drain_legacy_rollback_fence(app_server, thread_id, request_id)
            .await?;
        let canonical = match response {
            Some(thread) => thread,
            None => app_server
                .thread_read(thread_id, /*include_turns*/ true)
                .await
                .wrap_err(
                    "history changed but refresh failed; reopen this session before continuing",
                )?,
        };
        if canonical.id != thread_id.to_string()
            || canonical.history_mode != ThreadHistoryMode::Legacy
            || canonical.turns.len() != selected
        {
            bail!(
                "history changed but its new boundary could not be verified; reopen this session before continuing"
            );
        }
        self.install_legacy_prompt_edit(thread_id, canonical, prompt)
            .await?;
        tui.frame_requester().schedule_frame();
        Ok(())
    }

    async fn drain_legacy_rollback_fence(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        request_id: RequestId,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            while let Some(event) = app_server.next_event().await {
                if matches!(&event, AppServerEvent::RequestCompleted { request_id: completed }
                    if completed == &request_id)
                {
                    return Ok(());
                }
                if matches!(
                    event,
                    AppServerEvent::Disconnected { .. } | AppServerEvent::Lagged { .. }
                ) {
                    bail!(
                        "the history reset boundary was lost; reopen this session before continuing"
                    );
                }
                // Other threads keep their events. This thread's pre-fence suffix must not
                // reach either its live widget or replay store.
                if !event_targets_thread(&event, thread_id) {
                    self.handle_app_server_event(app_server, event).await;
                }
            }
            Err(eyre!("app-server disconnected during history reset"))
        })
        .await
        .wrap_err("history changed but refresh timed out; reopen this session before continuing")?
    }

    async fn install_legacy_prompt_edit(
        &mut self,
        thread_id: ThreadId,
        canonical: Thread,
        prompt: UserMessage,
    ) -> Result<()> {
        let mut session = self
            .thread_event_channels
            .get(&thread_id)
            .ok_or_else(|| eyre!("edited thread is no longer available"))?
            .store
            .lock()
            .await
            .session
            .clone()
            .ok_or_else(|| eyre!("edited thread has no session"))?;
        self.abort_thread_event_listener(thread_id);
        // The client fence was consumed; discard already-routed events rather than replaying
        // stale item completions. Removing the store also invalidates its replay buffer.
        self.active_thread_rx = None;
        self.thread_event_channels.remove(&thread_id);
        self.active_thread_id = None;
        session.rollout_path = canonical.path.clone();
        if self.primary_thread_id == Some(thread_id) {
            self.primary_session_configured = Some(session.clone());
        }
        self.chat_widget.restore_thread_input_state(
            /*input_state*/ None,
            crate::chatwidget::ThreadInputStateRestoreMode {
                preserve_in_flight_turn: false,
            },
        );
        self.recap.reset_for_new_thread(Instant::now());
        self.recap.seed_from_turns(&canonical.turns, Instant::now());
        self.retain_realtime_replay_state_before_replace();
        self.forget_realtime_replay_thread(thread_id);
        self.chat_widget
            .reset_after_prompt_revert(canonical.path.clone(), &canonical.turns);
        self.chat_widget.restore_user_message_to_composer(prompt);
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        {
            let mut store = self.ensure_thread_channel(thread_id).store.lock().await;
            store.set_session(session, canonical.turns.clone());
            store.turn_history_complete = true;
        }
        self.activate_thread_channel(thread_id).await;
        let visibility = if self.config.show_raw_agent_reasoning {
            crate::thread_transcript::RawReasoningVisibility::Visible
        } else {
            crate::thread_transcript::RawReasoningVisibility::Hidden
        };
        let mut config = self.config.clone();
        config.show_compact_summary = self.local_settings.tui.show_compact_summary;
        config.tui_agent_prompt_preview_lines = self.local_settings.tui.agent_prompt_preview_lines;
        config.tui_agent_response_preview_lines =
            self.local_settings.tui.agent_response_preview_lines;
        config.tui_command_output_preview_lines =
            self.local_settings.tui.command_output_preview_lines;
        config.tui_user_shell_output_preview_lines =
            self.local_settings.tui.user_shell_output_preview_lines;
        let mut cells = self
            .transcript_cells
            .iter()
            .rev()
            .find(|cell| cell.as_any().is::<crate::history_cell::SessionInfoCell>())
            .cloned()
            .into_iter()
            .collect::<Vec<_>>();
        cells.extend(crate::thread_transcript::thread_to_transcript_cells(
            canonical,
            visibility,
            Some(&config),
        ));
        // Widget shutdown can have queued transcript inserts. Install the authoritative
        // replacement after them, using the same input/reset barrier as Paginated revert.
        self.app_event_tx.send(AppEvent::FinishPromptRevert {
            thread_id,
            nth_user_message: 0,
            canonical_cells: Some(cells),
        });
        Ok(())
    }
}

fn event_targets_thread(event: &AppServerEvent, thread_id: ThreadId) -> bool {
    match event {
        AppServerEvent::ServerNotification(notification) => matches!(
            server_notification_thread_target(notification),
            ServerNotificationThreadTarget::Thread(target) if target == thread_id
        ),
        AppServerEvent::ServerRequest(request) => {
            server_request_thread_id(request) == Some(thread_id)
        }
        AppServerEvent::Lagged { .. }
        | AppServerEvent::Disconnected { .. }
        | AppServerEvent::RequestCompleted { .. } => false,
    }
}
