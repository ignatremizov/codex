//! Legacy prompt editing uses one guarded mutation and an ordered history-reset fence.
//! Failure after issuance leaves the affected thread read-only for this App's lifetime.

use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::app_server_event_targets::server_request_thread_id;
use super::*;
use crate::app_backtrack::LegacyRollbackTarget;
use crate::app_backtrack::PromptEditBoundary;
use crate::app_backtrack::PromptEditTarget;
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
            | AppEvent::ForkAgentsOverviewThread { thread_id }
            | AppEvent::RenameAgentsOverviewThread { thread_id, .. }
            | AppEvent::GeneratedThreadTitle { thread_id, .. }
            | AppEvent::StopAgentsOverviewThread { thread_id }
            | AppEvent::RunAgentsOverviewAction { thread_id, .. }
            | AppEvent::ApplyPermissionShortcut { thread_id, .. }
            | AppEvent::UpdateLunaReserveReasoning { thread_id, .. }
            | AppEvent::PersistDaybreakSelection { thread_id, .. }
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
        let selection: Result<PromptEditTarget> = async {
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
            let target = crate::app_backtrack::selected_prompt_target(
                &thread.turns,
                selected_cell
                    .as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .and_then(|cell| cell.identity.get()),
                start_item.as_ref(),
                nth_user_message,
                &mut prompt,
            )?;
            if target.before_item_id.is_some() {
                app_server.require_prompt_item_boundaries().await?;
            }
            Ok(target)
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
                PromptEditBoundary {
                    turn_id: thread.turns[selected.turn_index].id.clone(),
                    before_item_id: selected.before_item_id,
                },
                prompt,
            )
            .await;
            return Ok(());
        }
        let target = LegacyRollbackTarget::new(&thread.turns, &selected)?;
        let retained_turn_count =
            selected.turn_index + usize::from(selected.before_item_id.is_some());
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
                // The draft was restored before admission; restoring again would append it.
                self.chat_widget
                    .add_error_message(format!("Failed to edit the selected prompt: {err:#}"));
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
            || canonical.turns.len() != retained_turn_count
        {
            bail!(
                "history changed but its new boundary could not be verified; reopen this session before continuing"
            );
        }
        self.install_prompt_edit(thread_id, canonical, prompt)
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
                if event.backend_id != app_server.backend_id() {
                    continue;
                }
                if matches!(&event.event, AppServerEvent::RequestCompleted { request_id: completed }
                    if completed == &request_id)
                {
                    return Ok(());
                }
                if matches!(
                    event.event,
                    AppServerEvent::Disconnected { .. } | AppServerEvent::Lagged { .. }
                ) {
                    bail!(
                        "the history reset boundary was lost; reopen this session before continuing"
                    );
                }
                // Other threads keep their events. This thread's pre-fence suffix must not
                // reach either its live widget or replay store.
                if !event_targets_thread(&event.event, thread_id) {
                    self.handle_backend_event(app_server, event).await;
                }
            }
            Err(eyre!("app-server disconnected during history reset"))
        })
        .await
        .wrap_err("history changed but refresh timed out; reopen this session before continuing")?
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
